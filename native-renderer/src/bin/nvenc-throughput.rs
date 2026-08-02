//! Measures the direct NVENC ceiling for a resident 2048x2048 NV12 frame ring.
//!
//! The frame data is uploaded once during setup. The timed section submits only
//! CUDA device pointers to NVENC and drains the resulting Annex-B bitstreams.

use std::{
    collections::VecDeque,
    ffi::c_void,
    fs::File,
    io::{self, Write},
    path::PathBuf,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use cudarc::driver::{CudaContext, result as cuda, sys as cuda_sys};
use nvidia_video_codec_sdk::{
    EncodePictureParams, Encoder, EncoderInitParams, ErrorKind,
    sys::nvEncodeAPI::{
        GUID, NV_ENC_AV1_PROFILE_MAIN_GUID, NV_ENC_BUFFER_FORMAT, NV_ENC_CODEC_AV1_GUID,
        NV_ENC_CODEC_H264_GUID, NV_ENC_H264_PROFILE_HIGH_GUID, NV_ENC_INPUT_RESOURCE_TYPE,
        NV_ENC_PARAMS_RC_MODE, NV_ENC_PRESET_P1_GUID, NV_ENC_QP, NV_ENC_TUNING_INFO,
        NVENC_INFINITE_GOPLENGTH,
    },
};
use serde::Serialize;

const WIDTH: u32 = 2_048;
const HEIGHT: u32 = 2_048;
const FRAMES: usize = 2_281;
const FRAMERATE: u32 = 30;
const RING_SIZE: usize = 16;
const QP: u32 = 18;

type DynError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone, Copy, Debug, Default)]
enum Codec {
    #[default]
    H264,
    Av1,
}

impl Codec {
    fn encode_guid(self) -> GUID {
        match self {
            Self::H264 => NV_ENC_CODEC_H264_GUID,
            Self::Av1 => NV_ENC_CODEC_AV1_GUID,
        }
    }

    fn profile_guid(self) -> GUID {
        match self {
            Self::H264 => NV_ENC_H264_PROFILE_HIGH_GUID,
            Self::Av1 => NV_ENC_AV1_PROFILE_MAIN_GUID,
        }
    }

    fn report_name(self) -> &'static str {
        match self {
            Self::H264 => "H.264 Annex B",
            Self::Av1 => "AV1 OBU",
        }
    }

    fn default_output_path(self) -> &'static str {
        match self {
            Self::H264 => "/tmp/nvenc-throughput.h264",
            Self::Av1 => "/tmp/nvenc-throughput.av1",
        }
    }
}

#[derive(Debug)]
struct Options {
    codec: Codec,
    output_path: Option<PathBuf>,
}

#[derive(Debug)]
struct SyncCudaAllocation {
    pointer: cuda_sys::CUdeviceptr,
    context: Arc<CudaContext>,
}

impl Drop for SyncCudaAllocation {
    fn drop(&mut self) {
        let result = self
            .context
            .bind_to_thread()
            .and_then(|()| unsafe { cuda::free_sync(self.pointer) });
        if let Err(error) = result {
            eprintln!("failed to free NVENC benchmark CUDA allocation: {error}");
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Utilization {
    gpu_percent: f64,
    encoder_percent: f64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Report {
    codec: &'static str,
    input_format: &'static str,
    width: u32,
    height: u32,
    frames: usize,
    framerate: u32,
    preset: &'static str,
    tuning: &'static str,
    rate_control: &'static str,
    constant_qp: u32,
    ring_size: usize,
    setup_seconds: f64,
    encode_and_drain_seconds: f64,
    frames_per_second: f64,
    output_bytes: u64,
    output_path: Option<PathBuf>,
    encoder_busy_retries: u64,
    need_more_input_count: u64,
    utilization_before_encode: Option<Utilization>,
    utilization_after_encode: Option<Utilization>,
    utilization_sample_count: usize,
    average_gpu_percent_during_encode: Option<f64>,
    maximum_gpu_percent_during_encode: Option<f64>,
    average_encoder_percent_during_encode: Option<f64>,
    maximum_encoder_percent_during_encode: Option<f64>,
}

fn read_utilization() -> Option<Utilization> {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=utilization.gpu,utilization.encoder",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = std::str::from_utf8(&output.stdout).ok()?;
    let mut values = text.lines().next()?.split(',').map(str::trim);
    Some(Utilization {
        gpu_percent: values.next()?.parse().ok()?,
        encoder_percent: values.next()?.parse().ok()?,
    })
}

fn start_utilization_monitor() -> (Arc<AtomicBool>, thread::JoinHandle<Vec<Utilization>>) {
    let running = Arc::new(AtomicBool::new(true));
    let thread_running = Arc::clone(&running);
    let handle = thread::spawn(move || {
        let mut samples = Vec::new();
        while thread_running.load(Ordering::Relaxed) {
            if let Some(sample) = read_utilization() {
                samples.push(sample);
            }
            thread::sleep(Duration::from_millis(50));
        }
        samples
    });
    (running, handle)
}

fn neutral_nv12_frame() -> Vec<u8> {
    let luma_bytes = (WIDTH * HEIGHT) as usize;
    let mut frame = vec![16_u8; luma_bytes + luma_bytes / 2];
    frame[luma_bytes..].fill(128);
    frame
}

fn parse_options() -> Result<Options, DynError> {
    let mut codec = Codec::H264;
    let mut output_path = None;
    let mut no_output = false;
    for value in std::env::args_os().skip(1) {
        if value == "--av1" {
            codec = Codec::Av1;
        } else if value == "--no-output" {
            no_output = true;
        } else if output_path.replace(PathBuf::from(value)).is_some() {
            return Err("only one output path may be specified".into());
        }
    }
    if no_output && output_path.is_some() {
        return Err("--no-output cannot be combined with an output path".into());
    }
    Ok(Options {
        codec,
        output_path: if no_output {
            None
        } else {
            Some(output_path.unwrap_or_else(|| PathBuf::from(codec.default_output_path())))
        },
    })
}

fn summarize_utilization(
    samples: &[Utilization],
) -> (Option<f64>, Option<f64>, Option<f64>, Option<f64>) {
    if samples.is_empty() {
        return (None, None, None, None);
    }
    let count = samples.len() as f64;
    let average_gpu = samples.iter().map(|sample| sample.gpu_percent).sum::<f64>() / count;
    let maximum_gpu = samples
        .iter()
        .map(|sample| sample.gpu_percent)
        .fold(0.0_f64, f64::max);
    let average_encoder = samples
        .iter()
        .map(|sample| sample.encoder_percent)
        .sum::<f64>()
        / count;
    let maximum_encoder = samples
        .iter()
        .map(|sample| sample.encoder_percent)
        .fold(0.0_f64, f64::max);
    (
        Some(average_gpu),
        Some(maximum_gpu),
        Some(average_encoder),
        Some(maximum_encoder),
    )
}

fn main() -> Result<(), DynError> {
    let options = parse_options()?;
    let codec = options.codec;
    let output_path = options.output_path;
    let setup_started = Instant::now();

    let cuda_context = CudaContext::new(0)?;
    let encoder = Encoder::initialize_with_cuda(Arc::clone(&cuda_context))?;

    let encode_guid = codec.encode_guid();
    let preset_guid = NV_ENC_PRESET_P1_GUID;
    let tuning = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    let format = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12;
    if !encoder.get_encode_guids()?.contains(&encode_guid) {
        return Err(format!("GPU does not support NVENC {}", codec.report_name()).into());
    }
    if !encoder
        .get_preset_guids(encode_guid)?
        .contains(&preset_guid)
    {
        return Err(format!(
            "GPU does not support the NVENC P1 preset for {}",
            codec.report_name()
        )
        .into());
    }
    if !encoder
        .get_supported_input_formats(encode_guid)?
        .contains(&format)
    {
        return Err(format!(
            "GPU does not support NV12 input for NVENC {}",
            codec.report_name()
        )
        .into());
    }

    let mut preset = encoder.get_preset_config(encode_guid, preset_guid, tuning)?;
    preset.presetCfg.profileGUID = codec.profile_guid();
    preset.presetCfg.gopLength = NVENC_INFINITE_GOPLENGTH;
    preset.presetCfg.frameIntervalP = 1;
    preset.presetCfg.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CONSTQP;
    preset.presetCfg.rcParams.constQP = NV_ENC_QP {
        qpInterP: QP,
        qpInterB: QP,
        qpIntra: QP,
    };
    preset.presetCfg.rcParams.lookaheadDepth = 0;
    preset.presetCfg.rcParams.set_enableLookahead(0);
    preset.presetCfg.rcParams.set_zeroReorderDelay(1);
    if matches!(codec, Codec::Av1) {
        // P1's AV1 preset may suppress the sequence header. A standalone OBU
        // stream must carry it so decoders can discover the coded dimensions.
        unsafe {
            preset
                .presetCfg
                .encodeCodecConfig
                .av1Config
                .set_disableSeqHdr(0);
            preset
                .presetCfg
                .encodeCodecConfig
                .av1Config
                .set_repeatSeqHdr(1);
            preset
                .presetCfg
                .encodeCodecConfig
                .av1Config
                .set_outputAnnexBFormat(0);
        }
    }

    let mut init = EncoderInitParams::new(encode_guid, WIDTH, HEIGHT);
    init.preset_guid(preset_guid)
        .tuning_info(tuning)
        .display_aspect_ratio(WIDTH, HEIGHT)
        .framerate(FRAMERATE, 1)
        .enable_picture_type_decision()
        .encode_config(&mut preset.presetCfg);
    let session = encoder.start_session(format, init)?;

    // Upload each ring slot exactly once, before timing. Every encode submission
    // below refers directly to one of these persistent CUDA allocations.
    let host_frame = neutral_nv12_frame();
    let mut available_inputs = Vec::with_capacity(RING_SIZE);
    for _ in 0..RING_SIZE {
        // NVENC 12.1 rejects stream-ordered cuMemAllocAsync allocations on
        // this driver, so use the traditional allocation API it documents.
        // The only upload is synchronous and occurs outside the timed region.
        let device_pointer = unsafe { cuda::malloc_sync(host_frame.len())? };
        let allocation = SyncCudaAllocation {
            pointer: device_pointer,
            context: Arc::clone(&cuda_context),
        };
        unsafe { cuda::memcpy_htod_sync(allocation.pointer, &host_frame)? };
        let registered = session.register_generic_resource(
            allocation,
            NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
            device_pointer as usize as *mut c_void,
            WIDTH,
        )?;
        available_inputs.push(registered);
    }

    let mut available_outputs = (0..RING_SIZE)
        .map(|_| session.create_output_bitstream())
        .collect::<Result<Vec<_>, _>>()?;
    let mut in_use = VecDeque::with_capacity(RING_SIZE);
    let mut output_file = output_path.as_ref().map(File::create).transpose()?;
    let setup_seconds = setup_started.elapsed().as_secs_f64();
    let utilization_before_encode = read_utilization();
    let (monitor_running, monitor) = start_utilization_monitor();

    let encode_started = Instant::now();
    let mut output_bytes = 0_u64;
    let mut drained_frames = 0_usize;
    let mut encoder_busy_retries = 0_u64;
    let mut need_more_input_count = 0_u64;

    for frame_index in 0..FRAMES {
        let mut input = available_inputs
            .pop()
            .expect("ring accounting guarantees a free CUDA input");
        let mut output = available_outputs
            .pop()
            .expect("ring accounting guarantees a free bitstream");

        let output_became_available = loop {
            let params = EncodePictureParams {
                input_timestamp: frame_index as u64,
                ..Default::default()
            };
            match session.encode_picture(&mut input, &mut output, params) {
                Ok(()) => {
                    in_use.push_back((input, output));
                    break true;
                }
                Err(error) if error.kind() == ErrorKind::NeedMoreInput => {
                    need_more_input_count += 1;
                    in_use.push_back((input, output));
                    break false;
                }
                Err(error) if error.kind() == ErrorKind::EncoderBusy => {
                    encoder_busy_retries += 1;
                    thread::yield_now();
                }
                Err(error) => return Err(error.into()),
            }
        };

        if in_use.len() < RING_SIZE {
            continue;
        }
        if !output_became_available {
            return Err(format!(
                "NVENC exhausted the {RING_SIZE}-slot ring before producing output"
            )
            .into());
        }
        let (input, mut output) = in_use
            .pop_front()
            .expect("a full ring always has an oldest submission");
        let lock = output.lock()?;
        output_bytes += lock.data().len() as u64;
        drained_frames += 1;
        if let Some(file) = output_file.as_mut() {
            file.write_all(lock.data())?;
        }
        drop(lock);
        available_inputs.push(input);
        available_outputs.push(output);
    }

    loop {
        match session.end_of_stream() {
            Ok(()) => break,
            Err(error) if error.kind() == ErrorKind::EncoderBusy => {
                encoder_busy_retries += 1;
                thread::yield_now();
            }
            Err(error) => return Err(error.into()),
        }
    }
    while let Some((_input, mut output)) = in_use.pop_front() {
        let lock = output.lock()?;
        output_bytes += lock.data().len() as u64;
        drained_frames += 1;
        if let Some(file) = output_file.as_mut() {
            file.write_all(lock.data())?;
        }
    }
    if let Some(file) = output_file.as_mut() {
        file.flush()?;
    }
    let encode_and_drain_seconds = encode_started.elapsed().as_secs_f64();

    monitor_running.store(false, Ordering::Relaxed);
    let utilization_samples = monitor
        .join()
        .map_err(|_| io::Error::other("utilization monitor panicked"))?;
    let utilization_after_encode = read_utilization();
    if drained_frames != FRAMES {
        return Err(
            format!("NVENC returned {drained_frames} frames for {FRAMES} submissions").into(),
        );
    }
    let (average_gpu, maximum_gpu, average_encoder, maximum_encoder) =
        summarize_utilization(&utilization_samples);

    let report = Report {
        codec: codec.report_name(),
        input_format: "NV12 CUDA device pointer",
        width: WIDTH,
        height: HEIGHT,
        frames: FRAMES,
        framerate: FRAMERATE,
        preset: "P1",
        tuning: "ultra-low-latency",
        rate_control: "constant QP",
        constant_qp: QP,
        ring_size: RING_SIZE,
        setup_seconds,
        encode_and_drain_seconds,
        frames_per_second: FRAMES as f64 / encode_and_drain_seconds,
        output_bytes,
        output_path,
        encoder_busy_retries,
        need_more_input_count,
        utilization_before_encode,
        utilization_after_encode,
        utilization_sample_count: utilization_samples.len(),
        average_gpu_percent_during_encode: average_gpu,
        maximum_gpu_percent_during_encode: maximum_gpu,
        average_encoder_percent_during_encode: average_encoder,
        maximum_encoder_percent_during_encode: maximum_encoder,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
