use std::borrow::Cow;
use std::num::NonZeroU32;
use std::sync::mpsc;
use std::time::Duration;

use crate::{Error, PIXI_COLOR_FORMAT, Result, SpritePipeline, TemporalTarget};

const COPY_ROW_ALIGNMENT: u32 = 256;
const READBACK_TIMEOUT: Duration = Duration::from_secs(30);

pub const NV12_SHADER: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) @interpolate(flat) layer: u32,
}

@group(0) @binding(0)
var source: texture_2d_array<f32>;

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @builtin(view_index) view_index: i32,
) -> VertexOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    var output: VertexOutput;
    output.position = vec4<f32>(positions[vertex_index], 0.0, 1.0);
    output.layer = u32(view_index);
    return output;
}

fn load_rgb(pixel: vec2<i32>, layer: i32) -> vec3<f32> {
    return textureLoad(source, pixel, layer, 0).rgb;
}

@fragment
fn fs_y(input: VertexOutput) -> @location(0) f32 {
    let pixel = vec2<i32>(input.position.xy);
    let rgb = load_rgb(pixel, i32(input.layer));
    return (16.0 / 255.0) + dot(
        rgb,
        vec3<f32>(0.18258588, 0.61423059, 0.06200706),
    );
}

@fragment
fn fs_uv(input: VertexOutput) -> @location(0) vec2<f32> {
    let pixel = vec2<i32>(input.position.xy) * 2;
    let layer = i32(input.layer);
    let rgb = (
        load_rgb(pixel, layer)
        + load_rgb(pixel + vec2<i32>(1, 0), layer)
        + load_rgb(pixel + vec2<i32>(0, 1), layer)
        + load_rgb(pixel + vec2<i32>(1, 1), layer)
    ) * 0.25;
    let u = (128.0 / 255.0) + dot(
        rgb,
        vec3<f32>(-0.10064373, -0.33857195, 0.43921569),
    );
    let v = (128.0 / 255.0) + dot(
        rgb,
        vec3<f32>(0.43921569, -0.39894216, -0.04027352),
    );
    return vec2<f32>(u, v);
}
"#;

/// Converts one source array layer into a single-plane NV12 image. The output
/// is an R8 texture with the luma plane in rows `0..height` and interleaved UV
/// bytes in rows `height..height * 3 / 2`. This packed representation matches
/// the two-dimensional CUDA array layout accepted by NVENC for NV12 input.
pub const PACKED_NV12_SHADER: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
}

@group(0) @binding(0)
var source: texture_2d<f32>;

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    var output: VertexOutput;
    output.position = vec4<f32>(positions[vertex_index], 0.0, 1.0);
    return output;
}

fn load_rgb(pixel: vec2<i32>) -> vec3<f32> {
    return textureLoad(source, pixel, 0).rgb;
}

@fragment
fn fs_y(input: VertexOutput) -> @location(0) f32 {
    let pixel = vec2<i32>(input.position.xy);
    let rgb = load_rgb(pixel);
    return (16.0 / 255.0) + dot(
        rgb,
        vec3<f32>(0.18258588, 0.61423059, 0.06200706),
    );
}

@fragment
fn fs_uv_packed(input: VertexOutput) -> @location(0) f32 {
    let output_pixel = vec2<i32>(input.position.xy);
    let source_height = i32(textureDimensions(source).y);
    let chroma_pixel = vec2<i32>(output_pixel.x / 2, output_pixel.y - source_height);
    let pixel = chroma_pixel * 2;
    let rgb = (
        load_rgb(pixel)
        + load_rgb(pixel + vec2<i32>(1, 0))
        + load_rgb(pixel + vec2<i32>(0, 1))
        + load_rgb(pixel + vec2<i32>(1, 1))
    ) * 0.25;
    let u = (128.0 / 255.0) + dot(
        rgb,
        vec3<f32>(-0.10064373, -0.33857195, 0.43921569),
    );
    let v = (128.0 / 255.0) + dot(
        rgb,
        vec3<f32>(0.43921569, -0.39894216, -0.04027352),
    );
    return select(u, v, output_pixel.x % 2 == 1);
}
"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Nv12ReadbackLayout {
    width: u32,
    height: u32,
    layers: NonZeroU32,
    row_stride: u32,
    y_plane_bytes: u64,
    frame_stride: u64,
    total_bytes: u64,
    tight_frame_bytes: usize,
}

impl Nv12ReadbackLayout {
    pub fn new(width: u32, height: u32, layers: NonZeroU32) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(Error::Invalid(
                "NV12 dimensions must be positive".to_owned(),
            ));
        }
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(Error::Invalid("NV12 dimensions must be even".to_owned()));
        }
        let row_stride = width
            .checked_add(COPY_ROW_ALIGNMENT - 1)
            .map(|value| value / COPY_ROW_ALIGNMENT * COPY_ROW_ALIGNMENT)
            .ok_or(Error::ArithmeticOverflow)?;
        let y_plane_bytes = u64::from(row_stride)
            .checked_mul(u64::from(height))
            .ok_or(Error::ArithmeticOverflow)?;
        let uv_plane_bytes = u64::from(row_stride)
            .checked_mul(u64::from(height / 2))
            .ok_or(Error::ArithmeticOverflow)?;
        let frame_stride = y_plane_bytes
            .checked_add(uv_plane_bytes)
            .ok_or(Error::ArithmeticOverflow)?;
        let total_bytes = frame_stride
            .checked_mul(u64::from(layers.get()))
            .ok_or(Error::ArithmeticOverflow)?;
        let tight_frame_bytes = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(3))
            .map(|bytes| bytes / 2)
            .ok_or(Error::ArithmeticOverflow)?;
        Ok(Self {
            width,
            height,
            layers,
            row_stride,
            y_plane_bytes,
            frame_stride,
            total_bytes,
            tight_frame_bytes,
        })
    }

    pub const fn width(self) -> u32 {
        self.width
    }

    pub const fn height(self) -> u32 {
        self.height
    }

    pub const fn layers(self) -> NonZeroU32 {
        self.layers
    }

    pub const fn row_stride(self) -> u32 {
        self.row_stride
    }

    pub const fn y_plane_bytes(self) -> u64 {
        self.y_plane_bytes
    }

    pub const fn frame_stride(self) -> u64 {
        self.frame_stride
    }

    pub const fn total_bytes(self) -> u64 {
        self.total_bytes
    }

    pub const fn tight_frame_bytes(self) -> usize {
        self.tight_frame_bytes
    }

    fn validate_mapped(self, mapped: &[u8], active_layers: u32) -> Result<()> {
        if active_layers == 0 || active_layers > self.layers.get() {
            return Err(Error::Invalid(
                "active NV12 layers are outside the readback target".to_owned(),
            ));
        }
        let used_bytes = self
            .frame_stride
            .checked_mul(u64::from(active_layers))
            .ok_or(Error::ArithmeticOverflow)?;
        if mapped.len() < usize::try_from(used_bytes).map_err(|_| Error::ArithmeticOverflow)? {
            return Err(Error::Invalid(
                "mapped NV12 readback is truncated".to_owned(),
            ));
        }
        Ok(())
    }

    fn visit_mapped<F>(
        self,
        mapped: &[u8],
        active_layers: u32,
        scratch: &mut Vec<u8>,
        mut visitor: F,
    ) -> Result<()>
    where
        F: FnMut(&[u8]) -> Result<()>,
    {
        self.validate_mapped(mapped, active_layers)?;
        let frame_stride =
            usize::try_from(self.frame_stride).map_err(|_| Error::ArithmeticOverflow)?;
        if self.row_stride == self.width {
            for layer in 0..active_layers {
                let start = (layer as usize)
                    .checked_mul(frame_stride)
                    .ok_or(Error::ArithmeticOverflow)?;
                let end = start
                    .checked_add(self.tight_frame_bytes)
                    .ok_or(Error::ArithmeticOverflow)?;
                let frame = mapped.get(start..end).ok_or_else(|| {
                    Error::Invalid("mapped NV12 readback is truncated".to_owned())
                })?;
                visitor(frame)?;
            }
            return Ok(());
        }

        scratch.resize(self.tight_frame_bytes, 0);
        for layer in 0..active_layers {
            let frame_offset = (layer as usize)
                .checked_mul(frame_stride)
                .ok_or(Error::ArithmeticOverflow)?;
            let mut destination_offset = 0usize;
            for row in 0..self.height {
                let row_offset = (row as usize)
                    .checked_mul(self.row_stride as usize)
                    .ok_or(Error::ArithmeticOverflow)?;
                let start = frame_offset
                    .checked_add(row_offset)
                    .ok_or(Error::ArithmeticOverflow)?;
                copy_mapped_row(
                    mapped,
                    scratch,
                    start,
                    &mut destination_offset,
                    self.width as usize,
                )?;
            }
            let uv_offset = frame_offset
                .checked_add(
                    usize::try_from(self.y_plane_bytes).map_err(|_| Error::ArithmeticOverflow)?,
                )
                .ok_or(Error::ArithmeticOverflow)?;
            for row in 0..self.height / 2 {
                let row_offset = (row as usize)
                    .checked_mul(self.row_stride as usize)
                    .ok_or(Error::ArithmeticOverflow)?;
                let start = uv_offset
                    .checked_add(row_offset)
                    .ok_or(Error::ArithmeticOverflow)?;
                copy_mapped_row(
                    mapped,
                    scratch,
                    start,
                    &mut destination_offset,
                    self.width as usize,
                )?;
            }
            if destination_offset != self.tight_frame_bytes {
                return Err(Error::Invalid(
                    "NV12 readback layout produced an invalid frame size".to_owned(),
                ));
            }
            visitor(scratch)?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn unpack(self, mapped: &[u8], active_layers: u32) -> Result<Vec<Vec<u8>>> {
        let mut frames = Vec::with_capacity(active_layers as usize);
        let mut scratch = Vec::new();
        self.visit_mapped(mapped, active_layers, &mut scratch, |frame| {
            frames.push(frame.to_vec());
            Ok(())
        })?;
        Ok(frames)
    }
}

fn copy_mapped_row(
    mapped: &[u8],
    destination: &mut [u8],
    source_offset: usize,
    destination_offset: &mut usize,
    width: usize,
) -> Result<()> {
    let source_end = source_offset
        .checked_add(width)
        .ok_or(Error::ArithmeticOverflow)?;
    let destination_end = destination_offset
        .checked_add(width)
        .ok_or(Error::ArithmeticOverflow)?;
    let source = mapped
        .get(source_offset..source_end)
        .ok_or_else(|| Error::Invalid("mapped NV12 readback is truncated".to_owned()))?;
    let target = destination
        .get_mut(*destination_offset..destination_end)
        .ok_or_else(|| Error::Invalid("NV12 readback destination is truncated".to_owned()))?;
    target.copy_from_slice(source);
    *destination_offset = destination_end;
    Ok(())
}

pub struct Nv12BatchConverter {
    y_texture: wgpu::Texture,
    y_view: wgpu::TextureView,
    uv_texture: wgpu::Texture,
    uv_view: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
    y_pipeline: wgpu::RenderPipeline,
    uv_pipeline: wgpu::RenderPipeline,
    layout: Nv12ReadbackLayout,
    source_identity: u64,
}

/// Converts the active layers of one temporal target into independent packed
/// R8 NV12 textures. Each destination must be `width x height * 3 / 2` and use
/// `TextureFormat::R8Unorm`.
pub struct PackedNv12Converter {
    bind_groups: Vec<wgpu::BindGroup>,
    y_pipeline: wgpu::RenderPipeline,
    uv_pipeline: wgpu::RenderPipeline,
    width: u32,
    height: u32,
    layers: NonZeroU32,
    source_identity: u64,
}

impl PackedNv12Converter {
    pub fn create(device: &wgpu::Device, source: &TemporalTarget) -> Result<Self> {
        if source.format != PIXI_COLOR_FORMAT {
            return Err(Error::Invalid(
                "packed NV12 conversion requires the Pixi RGBA8 source format".to_owned(),
            ));
        }
        Nv12ReadbackLayout::new(source.width, source.height, source.layers)?;
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("packed NV12 source layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("packed NV12 conversion pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("RGBA to packed BT.709 NV12 shader"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(PACKED_NV12_SHADER)),
        });
        let y_pipeline = packed_conversion_pipeline(
            device,
            &pipeline_layout,
            &shader,
            "fs_y",
            "packed NV12 Y conversion pipeline",
        );
        let uv_pipeline = packed_conversion_pipeline(
            device,
            &pipeline_layout,
            &shader,
            "fs_uv_packed",
            "packed NV12 UV conversion pipeline",
        );
        let bind_groups = (0..source.layers.get())
            .map(|layer| {
                let view = source.texture.create_view(&wgpu::TextureViewDescriptor {
                    label: Some("packed NV12 source layer"),
                    dimension: Some(wgpu::TextureViewDimension::D2),
                    base_array_layer: layer,
                    array_layer_count: Some(1),
                    ..Default::default()
                });
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("packed NV12 source binding"),
                    layout: &bind_group_layout,
                    entries: &[wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    }],
                })
            })
            .collect();
        Ok(Self {
            bind_groups,
            y_pipeline,
            uv_pipeline,
            width: source.width,
            height: source.height,
            layers: source.layers,
            source_identity: source.identity,
        })
    }

    pub const fn width(&self) -> u32 {
        self.width
    }

    pub const fn height(&self) -> u32 {
        self.height
    }

    pub const fn packed_height(&self) -> u32 {
        self.height + self.height / 2
    }

    pub const fn layers(&self) -> NonZeroU32 {
        self.layers
    }

    pub(crate) const fn source_identity(&self) -> u64 {
        self.source_identity
    }

    pub(crate) fn encode(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        destinations: &[&wgpu::TextureView],
        active_layers: u32,
    ) -> Result<()> {
        if active_layers == 0 || active_layers > self.layers.get() {
            return Err(Error::Invalid(
                "active packed NV12 layers are outside the conversion batch".to_owned(),
            ));
        }
        if destinations.len() < active_layers as usize {
            return Err(Error::Invalid(format!(
                "packed NV12 conversion needs {active_layers} destinations but received {}",
                destinations.len()
            )));
        }
        for (layer, destination) in destinations[..active_layers as usize].iter().enumerate() {
            let bind_group = &self.bind_groups[layer];
            encode_packed_conversion_pass(
                encoder,
                destination,
                bind_group,
                &self.y_pipeline,
                wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                [0.0, 0.0, self.width as f32, self.height as f32],
                "RGBA to packed NV12 Y",
            );
            encode_packed_conversion_pass(
                encoder,
                destination,
                bind_group,
                &self.uv_pipeline,
                wgpu::LoadOp::Load,
                [
                    0.0,
                    self.height as f32,
                    self.width as f32,
                    (self.height / 2) as f32,
                ],
                "RGBA to packed NV12 UV",
            );
        }
        Ok(())
    }
}

impl Nv12BatchConverter {
    pub fn create(device: &wgpu::Device, source: &TemporalTarget) -> Result<Self> {
        if source.format != PIXI_COLOR_FORMAT {
            return Err(Error::Invalid(
                "NV12 conversion requires the Pixi RGBA8 source format".to_owned(),
            ));
        }
        validate_converter_device(device.features(), source.layers)?;
        let layout = Nv12ReadbackLayout::new(source.width, source.height, source.layers)?;
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("NV12 source layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2Array,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("NV12 conversion pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("RGBA to BT.709 NV12 shader"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(NV12_SHADER)),
        });
        let y_pipeline = conversion_pipeline(
            device,
            &pipeline_layout,
            &shader,
            "fs_y",
            wgpu::TextureFormat::R8Unorm,
            source.layers,
            "NV12 Y conversion pipeline",
        );
        let uv_pipeline = conversion_pipeline(
            device,
            &pipeline_layout,
            &shader,
            "fs_uv",
            wgpu::TextureFormat::Rg8Unorm,
            source.layers,
            "NV12 UV conversion pipeline",
        );
        let y_texture = conversion_texture(
            device,
            source.width,
            source.height,
            source.layers,
            wgpu::TextureFormat::R8Unorm,
            "NV12 Y batch",
        );
        let y_view = array_view(&y_texture, source.layers, "NV12 Y batch view");
        let uv_texture = conversion_texture(
            device,
            source.width / 2,
            source.height / 2,
            source.layers,
            wgpu::TextureFormat::Rg8Unorm,
            "NV12 UV batch",
        );
        let uv_view = array_view(&uv_texture, source.layers, "NV12 UV batch view");
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("NV12 source binding"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&source.view),
            }],
        });
        Ok(Self {
            y_texture,
            y_view,
            uv_texture,
            uv_view,
            bind_group,
            y_pipeline,
            uv_pipeline,
            layout,
            source_identity: source.identity,
        })
    }

    pub const fn layout(&self) -> Nv12ReadbackLayout {
        self.layout
    }

    pub(crate) const fn source_identity(&self) -> u64 {
        self.source_identity
    }

    pub fn encode(&self, encoder: &mut wgpu::CommandEncoder) {
        encode_conversion_pass(
            encoder,
            &self.y_view,
            &self.bind_group,
            &self.y_pipeline,
            "RGBA to NV12 Y batch",
        );
        encode_conversion_pass(
            encoder,
            &self.uv_view,
            &self.bind_group,
            &self.uv_pipeline,
            "RGBA to NV12 UV batch",
        );
    }

    pub(crate) fn copy_to_readback<'r>(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        readback: &'r mut Nv12ReadbackBuffer,
        active_layers: u32,
    ) -> Result<Nv12ReadbackCopy<'r>> {
        if readback.layout != self.layout {
            return Err(Error::Invalid(
                "NV12 converter and readback layouts differ".to_owned(),
            ));
        }
        if active_layers == 0 || active_layers > self.layout.layers.get() {
            return Err(Error::Invalid(
                "active NV12 layers are outside the conversion batch".to_owned(),
            ));
        }
        for layer in 0..active_layers {
            let frame_offset = self
                .layout
                .frame_stride
                .checked_mul(u64::from(layer))
                .ok_or(Error::ArithmeticOverflow)?;
            copy_plane(
                encoder,
                &self.y_texture,
                layer,
                &readback.buffer,
                frame_offset,
                self.layout.row_stride,
                self.layout.width,
                self.layout.height,
            );
            copy_plane(
                encoder,
                &self.uv_texture,
                layer,
                &readback.buffer,
                frame_offset
                    .checked_add(self.layout.y_plane_bytes)
                    .ok_or(Error::ArithmeticOverflow)?,
                self.layout.row_stride,
                self.layout.width / 2,
                self.layout.height / 2,
            );
        }
        Ok(Nv12ReadbackCopy {
            readback,
            active_layers,
        })
    }
}

pub struct Nv12ReadbackBuffer {
    buffer: wgpu::Buffer,
    layout: Nv12ReadbackLayout,
    scratch: Vec<u8>,
}

/// Exclusive proof that one NV12 copy has been recorded for this buffer.
/// Submit the containing command encoder, then pass that exact submission to
/// `read`; the buffer cannot be copied or mapped again while this token exists.
#[must_use = "the recorded NV12 copy must be submitted and read"]
pub(crate) struct Nv12ReadbackCopy<'a> {
    readback: &'a mut Nv12ReadbackBuffer,
    active_layers: u32,
}

impl Nv12ReadbackBuffer {
    pub fn create(device: &wgpu::Device, layout: Nv12ReadbackLayout) -> Result<Self> {
        validate_readback_size(layout.total_bytes, device.limits().max_buffer_size)?;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("NV12 batch readback"),
            size: layout.total_bytes,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Ok(Self {
            buffer,
            layout,
            scratch: Vec::new(),
        })
    }

    pub const fn layout(&self) -> Nv12ReadbackLayout {
        self.layout
    }

    fn read_submitted(
        &mut self,
        device: &wgpu::Device,
        submission: wgpu::SubmissionIndex,
        active_layers: u32,
    ) -> Result<Vec<Vec<u8>>> {
        let mut frames = Vec::with_capacity(active_layers as usize);
        self.visit_submitted(device, submission, active_layers, |frame| {
            frames.push(frame.to_vec());
            Ok(())
        })?;
        Ok(frames)
    }

    fn visit_submitted<F>(
        &mut self,
        device: &wgpu::Device,
        submission: wgpu::SubmissionIndex,
        active_layers: u32,
        visitor: F,
    ) -> Result<()>
    where
        F: FnMut(&[u8]) -> Result<()>,
    {
        if active_layers == 0 || active_layers > self.layout.layers.get() {
            return Err(Error::Invalid(
                "active NV12 layers are outside the readback buffer".to_owned(),
            ));
        }
        let used_bytes = self
            .layout
            .frame_stride
            .checked_mul(u64::from(active_layers))
            .ok_or(Error::ArithmeticOverflow)?;
        let slice = self.buffer.slice(..used_bytes);
        let (sender, receiver) = mpsc::sync_channel(1);
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        let _unmap = BufferUnmapGuard(&self.buffer);
        if let Err(error) = device.poll(wgpu::PollType::Wait {
            submission_index: Some(submission),
            timeout: Some(READBACK_TIMEOUT),
        }) {
            return Err(Error::Invalid(format!("GPU readback poll failed: {error}")));
        }
        let map_result = receiver.recv_timeout(READBACK_TIMEOUT).map_err(|error| {
            Error::Invalid(match error {
                mpsc::RecvTimeoutError::Timeout => {
                    "GPU readback callback timed out after submission".to_owned()
                }
                mpsc::RecvTimeoutError::Disconnected => {
                    "GPU readback callback was dropped".to_owned()
                }
            })
        });
        let map_result = map_result?;
        if let Err(error) = map_result {
            return Err(Error::Invalid(format!(
                "GPU readback mapping failed: {error}"
            )));
        }
        let mapped = slice.get_mapped_range();
        self.layout
            .visit_mapped(&mapped, active_layers, &mut self.scratch, visitor)
    }
}

/// Unmap on every exit path, including a visitor error or panic. The mapped
/// range is created after this guard, so Rust drops that view before the guard.
struct BufferUnmapGuard<'a>(&'a wgpu::Buffer);

impl Drop for BufferUnmapGuard<'_> {
    fn drop(&mut self) {
        self.0.unmap();
    }
}

impl Nv12ReadbackCopy<'_> {
    /// Wait for the exact submission containing the recorded copy, remove GPU
    /// row padding, and return tightly packed frames in temporal-layer order.
    pub(crate) fn read(
        self,
        device: &wgpu::Device,
        submission: wgpu::SubmissionIndex,
    ) -> Result<Vec<Vec<u8>>> {
        self.readback
            .read_submitted(device, submission, self.active_layers)
    }

    /// Wait for the recorded copy and visit tightly packed frames in
    /// temporal-layer order without allocating a frame for each layer.
    pub(crate) fn visit<F>(
        self,
        device: &wgpu::Device,
        submission: wgpu::SubmissionIndex,
        visitor: F,
    ) -> Result<()>
    where
        F: FnMut(&[u8]) -> Result<()>,
    {
        self.readback
            .visit_submitted(device, submission, self.active_layers, visitor)
    }
}

fn validate_converter_device(features: wgpu::Features, layers: NonZeroU32) -> Result<()> {
    if !(SpritePipeline::MIN_VIEWS_PER_BATCH..=SpritePipeline::MAX_VIEWS_PER_BATCH)
        .contains(&layers.get())
    {
        return Err(Error::Invalid(format!(
            "NV12 conversion requires {} to {} multiview layers",
            SpritePipeline::MIN_VIEWS_PER_BATCH,
            SpritePipeline::MAX_VIEWS_PER_BATCH
        )));
    }
    if !features.contains(SpritePipeline::REQUIRED_FEATURES) {
        return Err(Error::Invalid(
            "GPU device lacks required multiview conversion support".to_owned(),
        ));
    }
    Ok(())
}

fn validate_readback_size(required: u64, maximum: u64) -> Result<()> {
    if required > maximum {
        return Err(Error::Invalid(format!(
            "NV12 readback requires {required} bytes but the GPU buffer limit is {maximum}"
        )));
    }
    Ok(())
}

fn conversion_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    fragment_entry: &str,
    format: wgpu::TextureFormat,
    layers: NonZeroU32,
    label: &str,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some(fragment_entry),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview: Some(layers),
        cache: None,
    })
}

fn packed_conversion_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    fragment_entry: &str,
    label: &str,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some(fragment_entry),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::R8Unorm,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview: None,
        cache: None,
    })
}

fn conversion_texture(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    layers: NonZeroU32,
    format: wgpu::TextureFormat,
    label: &str,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: layers.get(),
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

fn array_view(texture: &wgpu::Texture, layers: NonZeroU32, label: &str) -> wgpu::TextureView {
    texture.create_view(&wgpu::TextureViewDescriptor {
        label: Some(label),
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        array_layer_count: Some(layers.get()),
        ..Default::default()
    })
}

fn encode_conversion_pass(
    encoder: &mut wgpu::CommandEncoder,
    target: &wgpu::TextureView,
    bind_group: &wgpu::BindGroup,
    pipeline: &wgpu::RenderPipeline,
    label: &str,
) {
    let attachment = Some(wgpu::RenderPassColorAttachment {
        view: target,
        depth_slice: None,
        resolve_target: None,
        ops: wgpu::Operations {
            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
            store: wgpu::StoreOp::Store,
        },
    });
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
        color_attachments: &[attachment],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    });
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, bind_group, &[]);
    pass.draw(0..3, 0..1);
}

#[allow(clippy::too_many_arguments)]
fn encode_packed_conversion_pass(
    encoder: &mut wgpu::CommandEncoder,
    target: &wgpu::TextureView,
    bind_group: &wgpu::BindGroup,
    pipeline: &wgpu::RenderPipeline,
    load: wgpu::LoadOp<wgpu::Color>,
    viewport: [f32; 4],
    label: &str,
) {
    let attachment = Some(wgpu::RenderPassColorAttachment {
        view: target,
        depth_slice: None,
        resolve_target: None,
        ops: wgpu::Operations {
            load,
            store: wgpu::StoreOp::Store,
        },
    });
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
        color_attachments: &[attachment],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    });
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, bind_group, &[]);
    pass.set_viewport(viewport[0], viewport[1], viewport[2], viewport[3], 0.0, 1.0);
    pass.draw(0..3, 0..1);
}

#[allow(clippy::too_many_arguments)]
fn copy_plane(
    encoder: &mut wgpu::CommandEncoder,
    texture: &wgpu::Texture,
    layer: u32,
    buffer: &wgpu::Buffer,
    offset: u64,
    bytes_per_row: u32,
    width: u32,
    height: u32,
) {
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d {
                x: 0,
                y: 0,
                z: layer,
            },
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
}

pub fn validate_nv12_shader() -> Result<()> {
    validate_shader_source(NV12_SHADER, "NV12 conversion")?;
    validate_shader_source(PACKED_NV12_SHADER, "packed NV12 conversion")?;
    Ok(())
}

fn validate_shader_source(source: &str, label: &str) -> Result<()> {
    let module = naga::front::wgsl::parse_str(source)
        .map_err(|error| Error::Invalid(format!("{label} WGSL is invalid: {error}")))?;
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .map_err(|error| Error::Invalid(format!("{label} WGSL is unsupported: {error:#?}")))?;
    Ok(())
}

pub fn rgba8_to_nv12_reference(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    let layout =
        Nv12ReadbackLayout::new(width, height, NonZeroU32::new(1).expect("one is nonzero"))?;
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(Error::ArithmeticOverflow)?;
    if rgba.len() != expected {
        return Err(Error::Invalid(
            "RGBA reference frame has the wrong byte length".to_owned(),
        ));
    }
    let mut output = vec![0; layout.tight_frame_bytes];
    let y_len = width as usize * height as usize;
    for y in 0..height {
        for x in 0..width {
            let [r, g, b] = rgb_at(rgba, width, x, y);
            output[(y * width + x) as usize] =
                quantize(16.0 + 0.18258588 * r + 0.614_230_6 * g + 0.06200706 * b);
        }
    }
    for y in 0..height / 2 {
        for x in 0..width / 2 {
            let mut rgb = [0.0; 3];
            for offset_y in 0..2 {
                for offset_x in 0..2 {
                    let sample = rgb_at(rgba, width, x * 2 + offset_x, y * 2 + offset_y);
                    for channel in 0..3 {
                        rgb[channel] += sample[channel] * 0.25;
                    }
                }
            }
            let offset = y_len + (y * width + x * 2) as usize;
            output[offset] =
                quantize(128.0 - 0.10064373 * rgb[0] - 0.33857195 * rgb[1] + 0.439_215_7 * rgb[2]);
            output[offset + 1] =
                quantize(128.0 + 0.439_215_7 * rgb[0] - 0.39894216 * rgb[1] - 0.04027352 * rgb[2]);
        }
    }
    Ok(output)
}

fn rgb_at(rgba: &[u8], width: u32, x: u32, y: u32) -> [f32; 3] {
    let offset = ((y * width + x) * 4) as usize;
    [
        f32::from(rgba[offset]),
        f32::from(rgba[offset + 1]),
        f32::from(rgba[offset + 2]),
    ]
}

fn quantize(value: f32) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::{
        Nv12ReadbackLayout, rgba8_to_nv12_reference, validate_converter_device,
        validate_nv12_shader, validate_readback_size,
    };
    use crate::SpritePipeline;

    #[test]
    fn readback_layout_aligns_rows_and_unpacks_temporal_layers() {
        let layout = Nv12ReadbackLayout::new(4, 2, NonZeroU32::new(2).unwrap()).unwrap();
        assert_eq!(layout.row_stride, 256);
        assert_eq!(layout.y_plane_bytes, 512);
        assert_eq!(layout.frame_stride, 768);
        assert_eq!(layout.total_bytes, 1536);
        assert_eq!(layout.tight_frame_bytes, 12);
        let mut mapped = vec![0; layout.total_bytes as usize];
        for layer in 0..2usize {
            let base = layer * layout.frame_stride as usize;
            mapped[base..base + 4].fill((layer * 3 + 1) as u8);
            mapped[base + 256..base + 260].fill((layer * 3 + 2) as u8);
            mapped[base + 512..base + 516].fill((layer * 3 + 3) as u8);
        }
        assert_eq!(
            layout.unpack(&mapped, 2).unwrap(),
            vec![
                vec![1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3],
                vec![4, 4, 4, 4, 5, 5, 5, 5, 6, 6, 6, 6],
            ]
        );
        assert!(Nv12ReadbackLayout::new(3, 2, NonZeroU32::new(1).unwrap()).is_err());
    }

    #[test]
    fn contiguous_readback_visits_borrowed_mapped_frames() {
        let layout = Nv12ReadbackLayout::new(256, 2, NonZeroU32::new(2).unwrap()).unwrap();
        assert_eq!(layout.row_stride, layout.width);
        assert_eq!(layout.frame_stride as usize, layout.tight_frame_bytes);
        let mut mapped = vec![0; layout.total_bytes as usize];
        mapped[..layout.tight_frame_bytes].fill(17);
        mapped[layout.tight_frame_bytes..].fill(29);
        let mapped_start = mapped.as_ptr() as usize;
        let mut scratch = Vec::new();
        let mut pointers = Vec::new();
        let mut first_bytes = Vec::new();

        layout
            .visit_mapped(&mapped, 2, &mut scratch, |frame| {
                pointers.push(frame.as_ptr() as usize);
                first_bytes.push(frame[0]);
                Ok(())
            })
            .unwrap();

        assert!(scratch.is_empty());
        assert_eq!(first_bytes, [17, 29]);
        assert_eq!(
            pointers,
            [mapped_start, mapped_start + layout.tight_frame_bytes]
        );
    }

    #[test]
    fn padded_readback_reuses_one_tight_scratch_frame() {
        let layout = Nv12ReadbackLayout::new(4, 2, NonZeroU32::new(2).unwrap()).unwrap();
        let mut mapped = vec![0; layout.total_bytes as usize];
        for layer in 0..2usize {
            let base = layer * layout.frame_stride as usize;
            mapped[base..base + 4].fill((layer * 3 + 1) as u8);
            mapped[base + 256..base + 260].fill((layer * 3 + 2) as u8);
            mapped[base + 512..base + 516].fill((layer * 3 + 3) as u8);
        }
        let mut scratch = Vec::new();
        let mut pointers = Vec::new();
        let mut frames = Vec::new();

        layout
            .visit_mapped(&mapped, 2, &mut scratch, |frame| {
                pointers.push(frame.as_ptr() as usize);
                frames.push(frame.to_vec());
                Ok(())
            })
            .unwrap();

        assert_eq!(scratch.len(), layout.tight_frame_bytes);
        assert_eq!(pointers[0], pointers[1]);
        assert_eq!(
            frames,
            [
                vec![1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3],
                vec![4, 4, 4, 4, 5, 5, 5, 5, 6, 6, 6, 6],
            ]
        );

        let first_allocation = scratch.as_ptr();
        layout
            .visit_mapped(&mapped, 1, &mut scratch, |_| Ok(()))
            .unwrap();
        assert_eq!(scratch.as_ptr(), first_allocation);
    }

    #[test]
    fn visitor_errors_stop_iteration_and_truncated_mappings_are_rejected() {
        let layout = Nv12ReadbackLayout::new(256, 2, NonZeroU32::new(2).unwrap()).unwrap();
        let mapped = vec![0; layout.total_bytes as usize];
        let mut scratch = Vec::new();
        let mut visits = 0;
        let error = layout
            .visit_mapped(&mapped, 2, &mut scratch, |_| {
                visits += 1;
                Err(crate::Error::Invalid("visitor stopped".to_owned()))
            })
            .unwrap_err();
        assert_eq!(visits, 1);
        assert_eq!(error.to_string(), "visitor stopped");
        assert!(
            layout
                .visit_mapped(&mapped[..mapped.len() - 1], 2, &mut scratch, |_| Ok(()))
                .is_err()
        );
    }

    #[test]
    fn bt709_limited_reference_matches_black_white_and_red() {
        assert_eq!(
            rgba8_to_nv12_reference(2, 2, &[0, 0, 0, 255].repeat(4)).unwrap(),
            [16, 16, 16, 16, 128, 128]
        );
        assert_eq!(
            rgba8_to_nv12_reference(2, 2, &[255, 255, 255, 255].repeat(4)).unwrap(),
            [235, 235, 235, 235, 128, 128]
        );
        assert_eq!(
            rgba8_to_nv12_reference(2, 2, &[255, 0, 0, 255].repeat(4)).unwrap(),
            [63, 63, 63, 63, 102, 240]
        );
    }

    #[test]
    fn conversion_shader_parses() {
        validate_nv12_shader().unwrap();
    }

    #[test]
    fn rejects_invalid_converter_requirements_before_gpu_calls() {
        assert!(
            validate_converter_device(
                SpritePipeline::REQUIRED_FEATURES,
                NonZeroU32::new(1).unwrap()
            )
            .is_err()
        );
        assert!(
            validate_converter_device(wgpu::Features::empty(), NonZeroU32::new(2).unwrap())
                .is_err()
        );
        validate_converter_device(
            SpritePipeline::REQUIRED_FEATURES,
            NonZeroU32::new(6).unwrap(),
        )
        .unwrap();
        validate_readback_size(1024, 1024).unwrap();
        assert!(validate_readback_size(1025, 1024).is_err());
    }
}
