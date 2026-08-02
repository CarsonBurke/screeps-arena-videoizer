//! Direct AV1 NVENC input from exportable packed-NV12 Vulkan images.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::fs::File;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::os::fd::AsRawFd;
use std::ptr;
use std::rc::Rc;
use std::sync::Arc;
use std::thread;

use cudarc::driver::{
    result::{self as cuda_result, DriverError as CudaDriverError},
    safe::CudaContext,
    sys as cuda_sys,
};
use nvidia_video_codec_sdk::{
    Bitstream, EncodePictureParams, Encoder, EncoderInitParams, ErrorKind,
    MappedRegisteredResource, PersistentRegisteredResource, Session,
    sys::nvEncodeAPI::{
        NV_ENC_AV1_PROFILE_MAIN_GUID, NV_ENC_BUFFER_FORMAT, NV_ENC_CODEC_AV1_GUID,
        NV_ENC_INPUT_RESOURCE_TYPE, NV_ENC_PARAMS_RC_MODE, NV_ENC_PRESET_P1_GUID, NV_ENC_QP,
        NV_ENC_SPLIT_ENCODE_MODE, NV_ENC_TUNING_INFO, NVENC_INFINITE_GOPLENGTH,
    },
};

use crate::{Rational, VulkanExternalNv12};

/// Configuration for the low-latency direct AV1 encoder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VulkanNvencConfig {
    pub width: u32,
    pub height: u32,
    pub frames_per_second: Rational,
    pub constant_qp: u32,
    pub cuda_device_ordinal: usize,
}

impl VulkanNvencConfig {
    pub fn new(width: u32, height: u32, frames_per_second: Rational) -> Self {
        Self {
            width,
            height,
            frames_per_second,
            constant_qp: 18,
            cuda_device_ordinal: 0,
        }
    }
}

/// Direct Vulkan/CUDA/NVENC failure.
#[derive(Debug, thiserror::Error)]
pub enum VulkanNvencError {
    #[error("invalid direct NVENC configuration: {0}")]
    InvalidConfig(String),
    #[error("CUDA operation failed: {0}")]
    Cuda(#[from] CudaDriverError),
    #[error("NVENC operation failed: {0}")]
    Nvenc(#[from] nvidia_video_codec_sdk::safe::EncodeError),
    #[error("failed to clone exported Vulkan memory descriptor: {0}")]
    Io(#[from] std::io::Error),
}

/// One encoded AV1 access unit and the ring slot that is now reusable.
#[derive(Debug)]
pub struct EncodedAv1Frame {
    pub slot_index: usize,
    pub frame_index: u64,
    pub data: Vec<u8>,
}

/// Owns the CUDA context and initialized NVENC session.
#[derive(Debug)]
pub struct VulkanNvencEncoder {
    session: Session,
    cuda_context: Arc<CudaContext>,
    config: VulkanNvencConfig,
}

impl VulkanNvencEncoder {
    pub fn new(config: VulkanNvencConfig) -> Result<Self, VulkanNvencError> {
        validate_config(config)?;
        let cuda_context = CudaContext::new(config.cuda_device_ordinal)?;
        let encoder = Encoder::initialize_with_cuda(Arc::clone(&cuda_context))?;
        let format = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12;
        if !encoder.get_encode_guids()?.contains(&NV_ENC_CODEC_AV1_GUID) {
            return Err(VulkanNvencError::InvalidConfig(
                "the selected GPU does not support AV1 NVENC".to_owned(),
            ));
        }
        if !encoder
            .get_preset_guids(NV_ENC_CODEC_AV1_GUID)?
            .contains(&NV_ENC_PRESET_P1_GUID)
        {
            return Err(VulkanNvencError::InvalidConfig(
                "the selected GPU does not support the AV1 P1 preset".to_owned(),
            ));
        }
        if !encoder
            .get_supported_input_formats(NV_ENC_CODEC_AV1_GUID)?
            .contains(&format)
        {
            return Err(VulkanNvencError::InvalidConfig(
                "the selected GPU does not support NV12 AV1 input".to_owned(),
            ));
        }

        let tuning = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
        let mut preset =
            encoder.get_preset_config(NV_ENC_CODEC_AV1_GUID, NV_ENC_PRESET_P1_GUID, tuning)?;
        preset.presetCfg.profileGUID = NV_ENC_AV1_PROFILE_MAIN_GUID;
        preset.presetCfg.gopLength = NVENC_INFINITE_GOPLENGTH;
        preset.presetCfg.frameIntervalP = 1;
        preset.presetCfg.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CONSTQP;
        preset.presetCfg.rcParams.constQP = NV_ENC_QP {
            qpInterP: config.constant_qp,
            qpInterB: config.constant_qp,
            qpIntra: config.constant_qp,
        };
        preset.presetCfg.rcParams.lookaheadDepth = 0;
        preset.presetCfg.rcParams.set_enableLookahead(0);
        preset.presetCfg.rcParams.set_zeroReorderDelay(1);
        // The low-overhead OBU stream must carry a sequence header so the
        // concurrent FFmpeg stream-copy muxer can build av1C metadata.
        unsafe {
            let av1 = &mut preset.presetCfg.encodeCodecConfig.av1Config;
            av1.set_disableSeqHdr(0);
            av1.set_repeatSeqHdr(1);
            av1.set_outputAnnexBFormat(0);
        }

        let fps_numerator = u32::try_from(config.frames_per_second.numerator()).map_err(|_| {
            VulkanNvencError::InvalidConfig("frame-rate numerator exceeds u32".to_owned())
        })?;
        let fps_denominator =
            u32::try_from(config.frames_per_second.denominator()).map_err(|_| {
                VulkanNvencError::InvalidConfig("frame-rate denominator exceeds u32".to_owned())
            })?;
        let mut init = EncoderInitParams::new(NV_ENC_CODEC_AV1_GUID, config.width, config.height);
        init.preset_guid(NV_ENC_PRESET_P1_GUID)
            .tuning_info(tuning)
            .display_aspect_ratio(config.width, config.height)
            .framerate(fps_numerator, fps_denominator)
            .enable_picture_type_decision()
            .split_encode_mode(NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_THREE_FORCED_MODE)
            .encode_config(&mut preset.presetCfg);
        let session = encoder.start_session(format, init)?;
        Ok(Self {
            session,
            cuda_context,
            config,
        })
    }

    pub fn create_ring(
        &self,
        targets: Vec<Arc<VulkanExternalNv12>>,
    ) -> Result<VulkanNvencRing<'_>, VulkanNvencError> {
        if targets.is_empty() {
            return Err(VulkanNvencError::InvalidConfig(
                "the direct NVENC ring cannot be empty".to_owned(),
            ));
        }
        let cuda_uuid = self.cuda_context.uuid()?.bytes.map(|byte| byte as u8);
        let mut slots = Vec::with_capacity(targets.len());
        for target in targets {
            if target.width() != self.config.width || target.height() != self.config.height {
                return Err(VulkanNvencError::InvalidConfig(format!(
                    "Vulkan NV12 target is {}x{} but encoder is {}x{}",
                    target.width(),
                    target.height(),
                    self.config.width,
                    self.config.height
                )));
            }
            if target.device_uuid() != cuda_uuid {
                return Err(VulkanNvencError::InvalidConfig(format!(
                    "Vulkan device UUID {} does not match CUDA device {} UUID {}",
                    format_uuid(target.device_uuid()),
                    self.config.cuda_device_ordinal,
                    format_uuid(cuda_uuid),
                )));
            }
            let imported = Rc::new(CudaExternalArray::import(
                &self.cuda_context,
                target,
                self.config.width,
                self.config.height,
            )?);
            let array = imported.array as *mut c_void;
            let registered = self.session.register_persistent_generic_resource(
                imported,
                NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDAARRAY,
                array,
                self.config.width,
            )?;
            let output = self.session.create_output_bitstream()?;
            slots.push(RingSlot {
                registered,
                mapped: None,
                output,
                state: SlotState::Free,
            });
        }
        Ok(VulkanNvencRing {
            session: &self.session,
            slots,
            submitted: VecDeque::new(),
            drainable_outputs: 0,
            next_frame_index: 0,
            finished: false,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlotState {
    Free,
    Reserved,
    Submitted { frame_index: u64 },
}

struct RingSlot<'a> {
    registered: Rc<PersistentRegisteredResource<'a, Rc<CudaExternalArray>>>,
    mapped: Option<MappedRegisteredResource<'a, Rc<CudaExternalArray>>>,
    output: Bitstream<'a>,
    state: SlotState,
}

/// Bounded registered-resource ring. A target remains reserved until its
/// corresponding output bitstream has been locked and copied out.
pub struct VulkanNvencRing<'a> {
    session: &'a Session,
    slots: Vec<RingSlot<'a>>,
    submitted: VecDeque<usize>,
    drainable_outputs: usize,
    next_frame_index: u64,
    finished: bool,
}

impl<'a> VulkanNvencRing<'a> {
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    pub fn available_slots(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.state == SlotState::Free)
            .count()
    }

    /// Reserve a free target before recording Vulkan writes into it.
    pub fn acquire_slot(&mut self) -> Option<usize> {
        let (index, slot) = self
            .slots
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| slot.state == SlotState::Free)?;
        slot.state = SlotState::Reserved;
        Some(index)
    }

    /// Return a reservation when rendering failed before submission.
    pub fn release_slot(&mut self, slot_index: usize) -> Result<(), VulkanNvencError> {
        let slot = self.slot_mut(slot_index)?;
        if slot.state != SlotState::Reserved {
            return Err(VulkanNvencError::InvalidConfig(format!(
                "NVENC slot {slot_index} is not reserved"
            )));
        }
        slot.state = SlotState::Free;
        Ok(())
    }

    /// Submit a host-synchronized Vulkan target to NVENC.
    pub fn submit(&mut self, slot_index: usize) -> Result<(), VulkanNvencError> {
        let frame_index = self.next_frame_index;
        let next_frame_index = frame_index
            .checked_add(1)
            .ok_or_else(|| VulkanNvencError::InvalidConfig("frame index overflow".to_owned()))?;
        let session = self.session;
        let slot = self.slot_mut(slot_index)?;
        if slot.state != SlotState::Reserved {
            return Err(VulkanNvencError::InvalidConfig(format!(
                "NVENC slot {slot_index} must be reserved before submission"
            )));
        }
        debug_assert!(slot.mapped.is_none());
        slot.mapped = Some(slot.registered.map()?);
        let output_ready = loop {
            match session.encode_picture(
                slot.mapped
                    .as_mut()
                    .expect("reserved NVENC slot was mapped before submission"),
                &mut slot.output,
                EncodePictureParams {
                    input_timestamp: frame_index,
                    ..Default::default()
                },
            ) {
                Ok(()) => break true,
                Err(error) if error.kind() == ErrorKind::NeedMoreInput => break false,
                Err(error) if error.kind() == ErrorKind::EncoderBusy => thread::yield_now(),
                Err(error) => {
                    drop(slot.mapped.take());
                    return Err(error.into());
                }
            }
        };
        slot.state = SlotState::Submitted { frame_index };
        self.submitted.push_back(slot_index);
        if output_ready {
            // One successful synchronous submission makes the entire queued
            // prefix lockable, including frames previously accepted with
            // NEED_MORE_INPUT for reordering.
            self.drainable_outputs = self.submitted.len();
        }
        self.next_frame_index = next_frame_index;
        Ok(())
    }

    /// Drain the oldest ready submitted frame, freeing its target.
    pub fn drain_oldest(&mut self) -> Result<Option<EncodedAv1Frame>, VulkanNvencError> {
        let Some(&slot_index) = self.submitted.front() else {
            return Ok(None);
        };
        if self.drainable_outputs == 0 {
            return Ok(None);
        }
        let frame = {
            let slot = self.slot_mut(slot_index)?;
            let SlotState::Submitted { frame_index } = slot.state else {
                return Err(VulkanNvencError::InvalidConfig(format!(
                    "NVENC submission queue references idle slot {slot_index}"
                )));
            };
            let lock = slot.output.lock()?;
            let data = lock.data().to_vec();
            drop(lock);
            // The SDK requires external input mappings to remain live until
            // the corresponding output bitstream has been locked.
            drop(slot.mapped.take());
            slot.state = SlotState::Free;
            EncodedAv1Frame {
                slot_index,
                frame_index,
                data,
            }
        };
        let submitted = self.submitted.pop_front();
        debug_assert_eq!(submitted, Some(slot_index));
        self.drainable_outputs -= 1;
        Ok(Some(frame))
    }

    /// Send EOS and return every remaining packet in submission order.
    pub fn finish(mut self) -> Result<Vec<EncodedAv1Frame>, VulkanNvencError> {
        self.send_eos()?;
        self.drainable_outputs = self.submitted.len();
        let mut frames = Vec::with_capacity(self.submitted.len());
        while let Some(frame) = self.drain_oldest()? {
            frames.push(frame);
        }
        self.finished = true;
        Ok(frames)
    }

    fn send_eos(&self) -> Result<(), VulkanNvencError> {
        loop {
            match self.session.end_of_stream() {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == ErrorKind::EncoderBusy => thread::yield_now(),
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn slot_mut(&mut self, slot_index: usize) -> Result<&mut RingSlot<'a>, VulkanNvencError> {
        let capacity = self.slots.len();
        self.slots.get_mut(slot_index).ok_or_else(|| {
            VulkanNvencError::InvalidConfig(format!(
                "NVENC slot {slot_index} is outside a {capacity}-slot ring"
            ))
        })
    }
}

impl Drop for VulkanNvencRing<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if self.send_eos().is_err() {
            return;
        }
        self.drainable_outputs = self.submitted.len();
        while !self.submitted.is_empty() {
            if self.drain_oldest().is_err() {
                break;
            }
        }
    }
}

#[derive(Debug)]
struct CudaExternalArray {
    array: cuda_sys::CUarray,
    mipmapped_array: cuda_sys::CUmipmappedArray,
    external_memory: cuda_sys::CUexternalMemory,
    context: Arc<CudaContext>,
    // CUDA consumes an OPAQUE_FD after successful import on Linux.
    _consumed_fd: ManuallyDrop<File>,
    _target: Arc<VulkanExternalNv12>,
}

impl CudaExternalArray {
    fn import(
        context: &Arc<CudaContext>,
        target: Arc<VulkanExternalNv12>,
        width: u32,
        height: u32,
    ) -> Result<Self, VulkanNvencError> {
        context.bind_to_thread()?;
        let file = File::from(target.try_clone_opaque_fd()?);
        let external_memory = unsafe { import_external_memory(&file, target.allocation_size()) }?;
        // CUDA consumes an OPAQUE_FD after a successful import. Prevent every
        // later error path from closing the descriptor a second time.
        let file = ManuallyDrop::new(file);
        let mut mipmapped_array = ptr::null_mut();
        let descriptor = cuda_sys::CUDA_EXTERNAL_MEMORY_MIPMAPPED_ARRAY_DESC {
            offset: 0,
            arrayDesc: cuda_sys::CUDA_ARRAY3D_DESCRIPTOR {
                Width: width as usize,
                Height: target.packed_height() as usize,
                Depth: 0,
                Format: cuda_sys::CUarray_format::CU_AD_FORMAT_UNSIGNED_INT8,
                NumChannels: 1,
                Flags: 0,
            },
            numLevels: 1,
            reserved: [0; 16],
        };
        if let Err(error) = unsafe {
            cuda_sys::cuExternalMemoryGetMappedMipmappedArray(
                &mut mipmapped_array,
                external_memory,
                &descriptor,
            )
        }
        .result()
        {
            unsafe {
                let _ = cuda_result::external_memory::destroy_external_memory(external_memory);
            }
            return Err(error.into());
        }
        let mut array = ptr::null_mut();
        if let Err(error) =
            unsafe { cuda_sys::cuMipmappedArrayGetLevel(&mut array, mipmapped_array, 0) }.result()
        {
            unsafe {
                let _ = cuda_sys::cuMipmappedArrayDestroy(mipmapped_array);
                let _ = cuda_result::external_memory::destroy_external_memory(external_memory);
            }
            return Err(error.into());
        }
        debug_assert_eq!(height + height / 2, target.packed_height());
        Ok(Self {
            array,
            mipmapped_array,
            external_memory,
            context: Arc::clone(context),
            _consumed_fd: file,
            _target: target,
        })
    }
}

impl Drop for CudaExternalArray {
    fn drop(&mut self) {
        let _ = self.context.bind_to_thread();
        unsafe {
            let _ = cuda_sys::cuMipmappedArrayDestroy(self.mipmapped_array);
            let _ = cuda_result::external_memory::destroy_external_memory(self.external_memory);
        }
    }
}

unsafe fn import_external_memory(
    file: &File,
    allocation_size: u64,
) -> Result<cuda_sys::CUexternalMemory, CudaDriverError> {
    let mut external_memory = MaybeUninit::uninit();
    let descriptor = cuda_sys::CUDA_EXTERNAL_MEMORY_HANDLE_DESC {
        type_: cuda_sys::CUexternalMemoryHandleType::CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD,
        handle: cuda_sys::CUDA_EXTERNAL_MEMORY_HANDLE_DESC_st__bindgen_ty_1 {
            fd: file.as_raw_fd(),
        },
        size: allocation_size,
        flags: cuda_sys::CUDA_EXTERNAL_MEMORY_DEDICATED,
        reserved: [0; 16],
    };
    unsafe { cuda_sys::cuImportExternalMemory(external_memory.as_mut_ptr(), &descriptor) }
        .result()?;
    Ok(unsafe { external_memory.assume_init() })
}

fn validate_config(config: VulkanNvencConfig) -> Result<(), VulkanNvencError> {
    if config.width == 0
        || config.height == 0
        || !config.width.is_multiple_of(2)
        || !config.height.is_multiple_of(2)
    {
        return Err(VulkanNvencError::InvalidConfig(
            "NV12 dimensions must be positive and even".to_owned(),
        ));
    }
    if config.frames_per_second == Rational::ZERO {
        return Err(VulkanNvencError::InvalidConfig(
            "frame rate must be positive".to_owned(),
        ));
    }
    if config.constant_qp > 63 {
        return Err(VulkanNvencError::InvalidConfig(
            "AV1 constant QP must be between 0 and 63".to_owned(),
        ));
    }
    Ok(())
}

fn format_uuid(uuid: [u8; 16]) -> String {
    uuid.iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::{VulkanNvencConfig, validate_config};
    use crate::Rational;

    #[test]
    fn validates_direct_nvenc_configuration() {
        let valid = VulkanNvencConfig::new(2_048, 2_048, Rational::new(30, 1).unwrap());
        validate_config(valid).unwrap();
        let mut invalid = valid;
        invalid.width = 2_047;
        assert!(validate_config(invalid).is_err());
        invalid = valid;
        invalid.constant_qp = 64;
        assert!(validate_config(invalid).is_err());
    }
}
