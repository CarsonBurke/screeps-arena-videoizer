use std::borrow::Cow;
use std::collections::BTreeSet;
use std::num::{NonZeroU32, NonZeroU64};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};

use bytemuck::{Pod, Zeroable};

use crate::{Error, Result, TemporalSpriteBatch, TextureAtlas, mip::downsample_rgba8};

/// Bundled Pixi/WebGL uses RGBA8 without automatic sRGB conversion.
pub const PIXI_COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
static TEMPORAL_TARGET_ID: AtomicU64 = AtomicU64::new(1);
static TEMPORAL_RENDERER_ID: AtomicU64 = AtomicU64::new(1);
static TEMPORAL_SUBMISSION_ID: AtomicU64 = AtomicU64::new(1);

fn next_identity(counter: &AtomicU64) -> Result<u64> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .map_err(|_| Error::ArithmeticOverflow)
}

pub const SPRITE_SHADER: &str = r#"
struct FrameConfig {
    instances_per_view: u32,
    active_views: u32,
    output_size: vec2<f32>,
}

struct SpriteInstance {
    transform_x: vec4<f32>,
    transform_y: vec4<f32>,
    size_anchor: vec4<f32>,
    uv_rect: vec4<f32>,
    tint_alpha: vec4<f32>,
    atlas_page: u32,
    visible: u32,
    blur: f32,
    has_blur_filter: u32,
}

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) corner: vec2<f32>,
    @location(1) tint_alpha: vec4<f32>,
    @location(2) @interpolate(flat) atlas_page: u32,
    @location(3) @interpolate(flat) uv_rect: vec4<f32>,
}

@group(0) @binding(0)
var<storage, read> instances: array<SpriteInstance>;

@group(0) @binding(1)
var<uniform> frame: FrameConfig;

@group(0) @binding(2)
var atlas: texture_2d_array<f32>;

@group(0) @binding(3)
var atlas_sampler: sampler;

const QUAD = array<vec2<f32>, 6>(
    vec2<f32>(0.0, 0.0),
    vec2<f32>(1.0, 0.0),
    vec2<f32>(0.0, 1.0),
    vec2<f32>(0.0, 1.0),
    vec2<f32>(1.0, 0.0),
    vec2<f32>(1.0, 1.0),
);

fn clamped_rect_sample_level(
    uv_rect: vec4<f32>,
    local: vec2<f32>,
    page: u32,
    level: u32,
) -> vec4<f32> {
    let atlas_size = vec2<i32>(textureDimensions(atlas).xy);
    let rect_min = vec2<i32>(round(uv_rect.xy * vec2<f32>(atlas_size)));
    let rect_max = vec2<i32>(round(uv_rect.zw * vec2<f32>(atlas_size)));
    let divisor = i32(1u << level);
    let level_min = rect_min / divisor;
    let level_size = max((rect_max - rect_min) / divisor, vec2<i32>(1));
    let texel = clamp(local, vec2<f32>(0.0), vec2<f32>(1.0))
        * vec2<f32>(level_size) - vec2<f32>(0.5);
    let base = vec2<i32>(floor(texel));
    let fraction = fract(texel);
    let lower = clamp(base, vec2<i32>(0), level_size - vec2<i32>(1));
    let upper = clamp(base + vec2<i32>(1), vec2<i32>(0), level_size - vec2<i32>(1));
    let p00 = level_min + vec2<i32>(lower.x, lower.y);
    let p10 = level_min + vec2<i32>(upper.x, lower.y);
    let p01 = level_min + vec2<i32>(lower.x, upper.y);
    let p11 = level_min + vec2<i32>(upper.x, upper.y);
    let top = mix(
        textureLoad(atlas, p00, i32(page), i32(level)),
        textureLoad(atlas, p10, i32(page), i32(level)),
        fraction.x,
    );
    let bottom = mix(
        textureLoad(atlas, p01, i32(page), i32(level)),
        textureLoad(atlas, p11, i32(page), i32(level)),
        fraction.x,
    );
    return mix(top, bottom, fraction.y);
}

fn clamped_rect_sample(
    uv_rect: vec4<f32>,
    local: vec2<f32>,
    page: u32,
) -> vec4<f32> {
    let atlas_size = vec2<f32>(textureDimensions(atlas).xy);
    let rect_size = max(
        (uv_rect.zw - uv_rect.xy) * atlas_size,
        vec2<f32>(1.0),
    );
    let texel_derivative_x = dpdx(local) * rect_size;
    let texel_derivative_y = dpdy(local) * rect_size;
    let footprint = max(
        dot(texel_derivative_x, texel_derivative_x),
        dot(texel_derivative_y, texel_derivative_y),
    );
    let lod = clamp(
        0.5 * log2(max(footprint, 1.0)),
        0.0,
        f32(textureNumLevels(atlas) - 1u),
    );
    let first_level = u32(floor(lod));
    let second_level = min(first_level + 1u, textureNumLevels(atlas) - 1u);
    let mip_fraction = fract(lod);
    let first_sample = clamped_rect_sample_level(
        uv_rect,
        local,
        page,
        first_level,
    );
    if second_level == first_level || mip_fraction == 0.0 {
        return first_sample;
    }
    let second_sample = clamped_rect_sample_level(
        uv_rect,
        local,
        page,
        second_level,
    );
    return mix(first_sample, second_sample, mip_fraction);
}

@vertex
fn vertex_main(
    @builtin(vertex_index) vertex_index: u32,
    @builtin(instance_index) instance_index: u32,
    @builtin(view_index) view_index: i32,
) -> VertexOutput {
    let view = u32(view_index);
    let instance = instances[view * frame.instances_per_view + instance_index];
    let corner = QUAD[vertex_index];
    let local = (corner - instance.size_anchor.zw) * instance.size_anchor.xy;
    let pixel_position = vec2<f32>(
        dot(instance.transform_x.xy, local) + instance.transform_x.z,
        dot(instance.transform_y.xy, local) + instance.transform_y.z,
    );
    let position = vec2<f32>(
        pixel_position.x / frame.output_size.x * 2.0 - 1.0,
        1.0 - pixel_position.y / frame.output_size.y * 2.0,
    );
    var output: VertexOutput;
    output.position = vec4<f32>(position, 0.0, 1.0);
    output.corner = corner;
    output.tint_alpha = select(
        vec4<f32>(0.0),
        instance.tint_alpha,
        instance.visible != 0u && view < frame.active_views,
    );
    output.atlas_page = instance.atlas_page;
    output.uv_rect = instance.uv_rect;
    return output;
}

@fragment
fn fragment_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let sampled = clamped_rect_sample(
        input.uv_rect,
        input.corner,
        input.atlas_page,
    );
    let alpha = input.tint_alpha.a;
    return vec4<f32>(
        sampled.rgb * input.tint_alpha.rgb * alpha,
        sampled.a * alpha,
    );
}
"#;

pub const SPRITE_BLUR_SHADER: &str = r#"
struct FrameConfig {
    instances_per_view: u32,
    active_views: u32,
    output_size: vec2<f32>,
}

struct SpriteInstance {
    transform_x: vec4<f32>,
    transform_y: vec4<f32>,
    size_anchor: vec4<f32>,
    uv_rect: vec4<f32>,
    tint_alpha: vec4<f32>,
    atlas_page: u32,
    visible: u32,
    blur: f32,
    has_blur_filter: u32,
}

struct BlurConfig {
    instance_index: u32,
    pass_kind: u32,
    _padding_0: u32,
    _padding_1: u32,
}

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) @interpolate(flat) layer: u32,
}

@group(0) @binding(0)
var source: texture_2d_array<f32>;

@group(0) @binding(1)
var<storage, read> instances: array<SpriteInstance>;

@group(0) @binding(2)
var<uniform> frame: FrameConfig;

@group(0) @binding(3)
var<uniform> config: BlurConfig;

@vertex
fn vertex_main(
    @builtin(vertex_index) vertex_index: u32,
    @builtin(view_index) view_index: i32,
) -> VertexOutput {
    let positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    var output: VertexOutput;
    output.position = vec4<f32>(positions[vertex_index], 0.0, 1.0);
    output.layer = u32(view_index);
    return output;
}

fn source_texel(coordinate: vec2<i32>, layer: i32) -> vec4<f32> {
    let extent = vec2<i32>(textureDimensions(source).xy);
    return textureLoad(
        source,
        clamp(coordinate, vec2<i32>(0), extent - vec2<i32>(1)),
        layer,
        0,
    );
}

fn source_linear(pixel: vec2<f32>, layer: i32) -> vec4<f32> {
    let base = vec2<i32>(floor(pixel));
    let fraction = fract(pixel);
    let top = mix(
        source_texel(base, layer),
        source_texel(base + vec2<i32>(1, 0), layer),
        fraction.x,
    );
    let bottom = mix(
        source_texel(base + vec2<i32>(0, 1), layer),
        source_texel(base + vec2<i32>(1, 1), layer),
        fraction.x,
    );
    return mix(top, bottom, fraction.y);
}

@fragment
fn fragment_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let instance =
        instances[input.layer * frame.instances_per_view + config.instance_index];
    let center = input.position.xy - vec2<f32>(0.5);
    let layer = i32(input.layer);
    // Preserve horizontal pass three for zero-strength multiview layers while
    // neighboring layers execute their vertical passes. The final pass then
    // evaluates horizontal pass four directly into the scene.
    if instance.blur == 0.0 && config.pass_kind == 0u {
        discard;
    }
    let strength = instance.blur / 4.0;
    let horizontal =
        config.pass_kind == 1u || (config.pass_kind == 2u && instance.blur == 0.0);
    let direction = select(
        vec2<f32>(0.0, strength),
        vec2<f32>(strength, 0.0),
        horizontal,
    );
    return
        source_linear(center - direction * 2.0, layer) * 0.153388
        + source_linear(center - direction, layer) * 0.221461
        + source_linear(center, layer) * 0.250301
        + source_linear(center + direction, layer) * 0.221461
        + source_linear(center + direction * 2.0, layer) * 0.153388;
}
"#;

/// GPU storage layout consumed once per `(view, instance)` pair.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct SpriteInstance {
    /// Full local-to-output-pixel affine rows: `[a, c, tx, reserved]` and
    /// `[b, d, ty, reserved]`. This retains shear from nested Pixi transforms.
    pub transform_x: [f32; 4],
    pub transform_y: [f32; 4],
    /// Intrinsic width/height followed by normalized anchor x/y.
    pub size_anchor: [f32; 4],
    pub uv_rect: [f32; 4],
    /// RGB multiplier and alpha.
    pub tint_alpha: [f32; 4],
    pub atlas_page: u32,
    pub visible: u32,
    /// Pixi BlurFilter strength in output pixels.
    pub blur: f32,
    /// Whether the display object owns a BlurFilter. This remains set at zero
    /// strength because Pixi still executes and quantizes that filter.
    pub has_blur_filter: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct FrameConfig {
    pub instances_per_view: u32,
    pub active_views: u32,
    pub output_size: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct SpriteBlurConfig {
    instance_index: u32,
    pass_kind: u32,
    padding: [u32; 2],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpriteBlendMode {
    Normal,
    Add,
    Multiply,
    Screen,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpriteDrawRun {
    pub layer_order: u32,
    pub blend_mode: SpriteBlendMode,
    pub has_blur_filter: bool,
    pub instances: Range<u32>,
}

pub struct SpritePipeline {
    pub bind_group_layout: wgpu::BindGroupLayout,
    pub pipeline: wgpu::RenderPipeline,
    pub add_pipeline: wgpu::RenderPipeline,
    pub multiply_pipeline: wgpu::RenderPipeline,
    pub screen_pipeline: wgpu::RenderPipeline,
}

pub struct GpuTextureAtlas {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub sampler: wgpu::Sampler,
    pub width: u32,
    pub height: u32,
    pub layers: u32,
    pub mip_levels: u32,
}

pub struct TemporalTarget {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub width: u32,
    pub height: u32,
    pub layers: NonZeroU32,
    pub format: wgpu::TextureFormat,
    pub(crate) identity: u64,
}

struct SpriteBlurPipeline {
    bind_group_layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
    final_pipeline: wgpu::RenderPipeline,
    config_stride: u32,
}

struct SpriteBlurScratch {
    ping: TemporalTarget,
    pong: TemporalTarget,
}

struct SpriteBlurSlot {
    ping_blur_bind_group: wgpu::BindGroup,
    pong_blur_bind_group: wgpu::BindGroup,
    _config_buffer: wgpu::Buffer,
}

struct TemporalBatchSlot {
    bind_group: wgpu::BindGroup,
    instance_buffer: wgpu::Buffer,
    frame_buffer: wgpu::Buffer,
    target: TemporalTarget,
    blur: SpriteBlurSlot,
}

pub struct TemporalSpriteRenderer {
    pipeline: SpritePipeline,
    blur_pipeline: SpriteBlurPipeline,
    blur_scratch: SpriteBlurScratch,
    slots: Vec<TemporalBatchSlot>,
    views_per_batch: NonZeroU32,
    max_instances_per_view: u32,
    identity: u64,
}

pub struct LeasedTerrainPhase<'a> {
    pub pipeline: &'a crate::TerrainPipeline,
    pub bindings: &'a crate::TerrainGpuBindings,
    pub batch: &'a crate::TemporalTerrainBatch,
    pub load: wgpu::LoadOp<wgpu::Color>,
}

pub struct TemporalRenderBatch<'a> {
    pub vector_pipeline: &'a crate::VectorPipeline,
    pub terrain_pipeline: &'a crate::TerrainPipeline,
    pub terrain_bindings: &'a crate::TerrainGpuBindings,
    pub compositor: &'a crate::TemporalLayerCompositor,
    pub terrain: &'a crate::TemporalTerrainSceneBatch,
    pub scene: &'a crate::TemporalSceneBatch,
    pub clear_color: wgpu::Color,
    /// Optional pre-rendered static terrain prefix and lighting layer. When
    /// present, `terrain` contains only the ordered dynamic remainder.
    pub terrain_cache: Option<TemporalTerrainCache<'a>>,
}

#[derive(Clone, Copy)]
pub struct TemporalTerrainCache<'a> {
    pub prefix: &'a TemporalTarget,
    pub lighting: Option<&'a crate::TemporalLightingSource>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct EncodedTemporalBatch {
    submission_id: u64,
    slot_index: usize,
    active_layers: Range<u32>,
    instances_per_view: u32,
    draw_runs: Vec<SpriteDrawRun>,
    slot_activations: Vec<u32>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct TemporalBatchLease {
    submission_id: u64,
    slot_index: usize,
}

impl EncodedTemporalBatch {
    pub fn active_layers(&self) -> Range<u32> {
        self.active_layers.clone()
    }

    pub const fn slot_index(&self) -> usize {
        self.slot_index
    }
}

/// Owns one command encoder and leases each temporal ring slot at most once.
/// `submit` executes the recorded work before the ring can be reused. Dropping
/// the value discards its command encoder; queued writes may then be
/// superseded by a later submission without producing output.
#[must_use = "temporal submissions must be submitted"]
pub struct TemporalSubmission<'a> {
    renderer: &'a mut TemporalSpriteRenderer,
    encoder: Option<wgpu::CommandEncoder>,
    first_slot: usize,
    next_slot: usize,
    submission_id: u64,
}

#[must_use = "the temporal submission and its NV12 readback must be completed together"]
pub struct PendingTemporalReadback<'r> {
    submission_id: u64,
    copy: crate::nv12::Nv12ReadbackCopy<'r>,
}

impl SpritePipeline {
    pub const REQUIRED_FEATURES: wgpu::Features = wgpu::Features::MULTIVIEW;
    pub const MIN_VIEWS_PER_BATCH: u32 = 2;
    /// Production Vulkan targets support at least this many views; wgpu still
    /// validates the adapter-specific limit when the pipelines are created.
    pub const MAX_VIEWS_PER_BATCH: u32 = 8;
    pub const MAX_IN_FLIGHT_BATCHES: u32 = 3;
    /// Main ring targets plus the two shared BlurFilter scratch arrays.
    /// Keeping this bounded prevents valid per-texture dimensions from
    /// exhausting ordinary GPUs before atlas/compositor resources exist.
    pub const MAX_TEMPORAL_COLOR_BYTES: u64 = 1024 * 1024 * 1024;

    pub fn create(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        views_per_batch: NonZeroU32,
    ) -> Result<Self> {
        validate_multiview_count(views_per_batch)?;
        if !device.features().contains(Self::REQUIRED_FEATURES) {
            return Err(Error::Invalid(
                "GPU device lacks required multiview rendering support".to_owned(),
            ));
        }
        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("temporal sprite bindings"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Storage { read_only: true },
                            has_dynamic_offset: false,
                            min_binding_size: NonZeroU64::new(
                                std::mem::size_of::<SpriteInstance>() as u64,
                            ),
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::VERTEX,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: NonZeroU64::new(
                                std::mem::size_of::<FrameConfig>() as u64
                            ),
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2Array,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("temporal sprite pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("temporal sprite shader"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(SPRITE_SHADER)),
        });
        let pipeline = create_sprite_pipeline(
            device,
            &pipeline_layout,
            &shader,
            target_format,
            views_per_batch,
            "temporal normal sprite pipeline",
            wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
        );
        let add_pipeline = create_sprite_pipeline(
            device,
            &pipeline_layout,
            &shader,
            target_format,
            views_per_batch,
            "temporal additive sprite pipeline",
            additive_blend(),
        );
        let multiply_pipeline = create_sprite_pipeline(
            device,
            &pipeline_layout,
            &shader,
            target_format,
            views_per_batch,
            "temporal multiply sprite pipeline",
            multiply_blend(),
        );
        let screen_pipeline = create_sprite_pipeline(
            device,
            &pipeline_layout,
            &shader,
            target_format,
            views_per_batch,
            "temporal screen sprite pipeline",
            screen_blend(),
        );
        Ok(Self {
            bind_group_layout,
            pipeline,
            add_pipeline,
            multiply_pipeline,
            screen_pipeline,
        })
    }

    fn for_blend_mode(&self, blend_mode: SpriteBlendMode) -> &wgpu::RenderPipeline {
        match blend_mode {
            SpriteBlendMode::Normal => &self.pipeline,
            SpriteBlendMode::Add => &self.add_pipeline,
            SpriteBlendMode::Multiply => &self.multiply_pipeline,
            SpriteBlendMode::Screen => &self.screen_pipeline,
        }
    }
}

impl SpriteBlurPipeline {
    fn create(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        views_per_batch: NonZeroU32,
    ) -> Result<Self> {
        let config_size = std::mem::size_of::<SpriteBlurConfig>() as u64;
        let config_alignment = device.limits().min_uniform_buffer_offset_alignment.max(1);
        let config_stride = u32::try_from(
            config_size
                .checked_next_multiple_of(u64::from(config_alignment))
                .ok_or(Error::ArithmeticOverflow)?,
        )
        .map_err(|_| Error::ArithmeticOverflow)?;
        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("temporal sprite blur bindings"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            view_dimension: wgpu::TextureViewDimension::D2Array,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Storage { read_only: true },
                            has_dynamic_offset: false,
                            min_binding_size: NonZeroU64::new(
                                std::mem::size_of::<SpriteInstance>() as u64,
                            ),
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: NonZeroU64::new(
                                std::mem::size_of::<FrameConfig>() as u64
                            ),
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: true,
                            min_binding_size: NonZeroU64::new(config_size),
                        },
                        count: None,
                    },
                ],
            });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("temporal sprite blur pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("temporal sprite blur shader"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(SPRITE_BLUR_SHADER)),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("temporal sprite blur pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fragment_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: Some(views_per_batch),
            cache: None,
        });
        let final_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("temporal sprite blur final pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fragment_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: Some(views_per_batch),
            cache: None,
        });
        Ok(Self {
            bind_group_layout,
            pipeline,
            final_pipeline,
            config_stride,
        })
    }
}

impl SpriteBlurSlot {
    fn create(
        device: &wgpu::Device,
        pipeline: &SpriteBlurPipeline,
        scratch: &SpriteBlurScratch,
        instance_buffer: &wgpu::Buffer,
        frame_buffer: &wgpu::Buffer,
        max_instances_per_view: u32,
    ) -> Result<Self> {
        let config_count = u64::from(max_instances_per_view)
            .checked_mul(3)
            .ok_or(Error::ArithmeticOverflow)?;
        let config_bytes = config_count
            .checked_mul(u64::from(pipeline.config_stride))
            .ok_or(Error::ArithmeticOverflow)?;
        if config_bytes > device.limits().max_buffer_size || config_bytes > u64::from(u32::MAX) {
            return Err(Error::Invalid(
                "temporal sprite blur configuration exceeds GPU buffer limits".to_owned(),
            ));
        }
        let config_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("temporal sprite blur configurations"),
            size: config_bytes,
            usage: wgpu::BufferUsages::UNIFORM,
            mapped_at_creation: true,
        });
        {
            let mut mapped = config_buffer.slice(..).get_mapped_range_mut();
            for instance_index in 0..max_instances_per_view {
                for pass_kind in 0..3 {
                    let config = SpriteBlurConfig {
                        instance_index,
                        pass_kind,
                        padding: [0; 2],
                    };
                    let config_index = u64::from(instance_index) * 3 + u64::from(pass_kind);
                    let offset = usize::try_from(
                        config_index
                            .checked_mul(u64::from(pipeline.config_stride))
                            .ok_or(Error::ArithmeticOverflow)?,
                    )
                    .map_err(|_| Error::ArithmeticOverflow)?;
                    let bytes = bytemuck::bytes_of(&config);
                    mapped[offset..offset + bytes.len()].copy_from_slice(bytes);
                }
            }
        }
        config_buffer.unmap();
        let blur_bind_group = |label, source: &wgpu::TextureView| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &pipeline.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(source),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: instance_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: frame_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &config_buffer,
                            offset: 0,
                            size: NonZeroU64::new(std::mem::size_of::<SpriteBlurConfig>() as u64),
                        }),
                    },
                ],
            })
        };
        let ping_blur_bind_group =
            blur_bind_group("temporal sprite blur from ping", &scratch.ping.view);
        let pong_blur_bind_group =
            blur_bind_group("temporal sprite blur from pong", &scratch.pong.view);
        Ok(Self {
            ping_blur_bind_group,
            pong_blur_bind_group,
            _config_buffer: config_buffer,
        })
    }
}

impl GpuTextureAtlas {
    pub fn upload(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        atlas: &TextureAtlas,
    ) -> Result<Self> {
        let first = atlas.pages.first().ok_or_else(|| {
            Error::Invalid("renderer texture atlas must contain at least one page".to_owned())
        })?;
        let width = first.width;
        let height = first.height;
        let layers = u32::try_from(atlas.pages.len()).map_err(|_| Error::ArithmeticOverflow)?;
        if width == 0 || height == 0 || layers == 0 {
            return Err(Error::Invalid(
                "renderer texture atlas dimensions must be positive".to_owned(),
            ));
        }
        let expected_bytes = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(Error::ArithmeticOverflow)?;
        if atlas.pages.iter().any(|page| {
            page.width != width || page.height != height || page.rgba.len() != expected_bytes
        }) {
            return Err(Error::Invalid(
                "renderer texture atlas pages must have one common RGBA extent".to_owned(),
            ));
        }
        if atlas.padding == 0 || !atlas.padding.is_power_of_two() {
            return Err(Error::Invalid(
                "renderer texture atlas padding must be a positive power of two".to_owned(),
            ));
        }
        let mip_levels = atlas_mip_level_count(atlas, width, height);

        let limits = device.limits();
        if width > limits.max_texture_dimension_2d
            || height > limits.max_texture_dimension_2d
            || layers > limits.max_texture_array_layers
        {
            return Err(Error::Invalid(
                "renderer texture atlas exceeds GPU device limits".to_owned(),
            ));
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("renderer texture atlas"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: layers,
            },
            mip_level_count: mip_levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: PIXI_COLOR_FORMAT,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        for (layer, page) in atlas.pages.iter().enumerate() {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: 0,
                        y: 0,
                        z: layer as u32,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                &page.rgba,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width * 4),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
        }
        let mip_chain = build_atlas_mip_levels(atlas, mip_levels)?;
        for (mip_index, mip_pages) in mip_chain.iter().enumerate() {
            let mip_level = u32::try_from(mip_index + 1).map_err(|_| Error::ArithmeticOverflow)?;
            let mip_width = (width >> mip_level).max(1);
            let mip_height = (height >> mip_level).max(1);
            for (layer, rgba) in mip_pages.iter().enumerate() {
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &texture,
                        mip_level,
                        origin: wgpu::Origin3d {
                            x: 0,
                            y: 0,
                            z: layer as u32,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    rgba,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(mip_width * 4),
                        rows_per_image: Some(mip_height),
                    },
                    wgpu::Extent3d {
                        width: mip_width,
                        height: mip_height,
                        depth_or_array_layers: 1,
                    },
                );
            }
        }
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("renderer texture atlas view"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            array_layer_count: Some(layers),
            ..Default::default()
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("renderer texture atlas sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            lod_max_clamp: (mip_levels - 1) as f32,
            ..Default::default()
        });
        Ok(Self {
            texture,
            view,
            sampler,
            width,
            height,
            layers,
            mip_levels,
        })
    }
}

fn atlas_mip_level_count(atlas: &TextureAtlas, width: u32, height: u32) -> u32 {
    let safe_from_padding = atlas.padding.ilog2() + 1;
    let available_from_extent = width.max(height).ilog2() + 1;
    safe_from_padding.min(available_from_extent)
}

fn build_atlas_mip_levels(atlas: &TextureAtlas, mip_levels: u32) -> Result<Vec<Vec<Vec<u8>>>> {
    let first = atlas.pages.first().ok_or_else(|| {
        Error::Invalid("renderer texture atlas must contain at least one page".to_owned())
    })?;
    let mut levels = (1..mip_levels)
        .map(|level| {
            let width = (first.width >> level).max(1);
            let height = (first.height >> level).max(1);
            let page_bytes = (width as usize)
                .checked_mul(height as usize)
                .and_then(|pixels| pixels.checked_mul(4))
                .ok_or(Error::ArithmeticOverflow)?;
            Ok(vec![vec![0; page_bytes]; atlas.pages.len()])
        })
        .collect::<Result<Vec<_>>>()?;
    for entry in atlas.entries.values() {
        let page = atlas.pages.get(entry.page as usize).ok_or_else(|| {
            Error::Invalid("renderer texture atlas entry page is out of bounds".to_owned())
        })?;
        if atlas.padding > 1 && (entry.x % atlas.padding != 0 || entry.y % atlas.padding != 0) {
            return Err(Error::Invalid(
                "renderer texture atlas entry is not mip-aligned".to_owned(),
            ));
        }
        let mut rgba = extract_rgba(
            &page.rgba,
            page.width,
            page.height,
            entry.x,
            entry.y,
            entry.width,
            entry.height,
        )?;
        let mut asset_width = entry.width;
        let mut asset_height = entry.height;
        for level in 1..mip_levels {
            rgba = downsample_rgba8(&rgba, asset_width, asset_height)?;
            asset_width = (asset_width / 2).max(1);
            asset_height = (asset_height / 2).max(1);
            let width = (first.width >> level).max(1);
            let height = (first.height >> level).max(1);
            let padding = (atlas.padding >> level).max(1);
            blit_wrapped_rgba(
                &mut levels[(level - 1) as usize][entry.page as usize],
                width,
                height,
                &rgba,
                asset_width,
                asset_height,
                entry.x >> level,
                entry.y >> level,
                padding,
            )?;
        }
    }
    Ok(levels)
}

fn extract_rgba(
    source: &[u8],
    source_width: u32,
    source_height: u32,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> Result<Vec<u8>> {
    if width == 0
        || height == 0
        || x.checked_add(width)
            .is_none_or(|right| right > source_width)
        || y.checked_add(height)
            .is_none_or(|bottom| bottom > source_height)
        || source.len() != source_width as usize * source_height as usize * 4
    {
        return Err(Error::Invalid(
            "renderer texture atlas entry pixels are invalid".to_owned(),
        ));
    }
    let mut output = Vec::with_capacity(width as usize * height as usize * 4);
    for row in y..y + height {
        let start = ((row * source_width + x) * 4) as usize;
        let end = start + width as usize * 4;
        output.extend_from_slice(&source[start..end]);
    }
    Ok(output)
}

#[allow(clippy::too_many_arguments)]
fn blit_wrapped_rgba(
    destination: &mut [u8],
    destination_width: u32,
    destination_height: u32,
    source: &[u8],
    source_width: u32,
    source_height: u32,
    x: u32,
    y: u32,
    padding: u32,
) -> Result<()> {
    if source.len() != source_width as usize * source_height as usize * 4
        || destination.len() != destination_width as usize * destination_height as usize * 4
    {
        return Err(Error::Invalid(
            "renderer texture mip pixels are invalid".to_owned(),
        ));
    }
    let start_x = x.saturating_sub(padding);
    let start_y = y.saturating_sub(padding);
    let end_x = x
        .checked_add(source_width)
        .and_then(|value| value.checked_add(padding))
        .unwrap_or(u32::MAX)
        .min(destination_width);
    let end_y = y
        .checked_add(source_height)
        .and_then(|value| value.checked_add(padding))
        .unwrap_or(u32::MAX)
        .min(destination_height);
    for destination_y in start_y..end_y {
        let relative_y = i64::from(destination_y) - i64::from(y);
        let source_y = relative_y.rem_euclid(i64::from(source_height)) as u32;
        for destination_x in start_x..end_x {
            let relative_x = i64::from(destination_x) - i64::from(x);
            let source_x = relative_x.rem_euclid(i64::from(source_width)) as u32;
            let source_offset = ((source_y * source_width + source_x) * 4) as usize;
            let destination_offset =
                ((destination_y * destination_width + destination_x) * 4) as usize;
            destination[destination_offset..destination_offset + 4]
                .copy_from_slice(&source[source_offset..source_offset + 4]);
        }
    }
    Ok(())
}

impl TemporalTarget {
    pub fn create(
        device: &wgpu::Device,
        width: u32,
        height: u32,
        layers: NonZeroU32,
        format: wgpu::TextureFormat,
    ) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(Error::Invalid(
                "temporal render dimensions must be positive".to_owned(),
            ));
        }
        let limits = device.limits();
        if width > limits.max_texture_dimension_2d
            || height > limits.max_texture_dimension_2d
            || layers.get() > limits.max_texture_array_layers
        {
            return Err(Error::Invalid(
                "temporal render target exceeds GPU device limits".to_owned(),
            ));
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("temporal RGBA frame batch"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: layers.get(),
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("temporal RGBA frame batch view"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            array_layer_count: Some(layers.get()),
            ..Default::default()
        });
        Ok(Self {
            texture,
            view,
            width,
            height,
            layers,
            format,
            identity: next_identity(&TEMPORAL_TARGET_ID)?,
        })
    }
}

impl TemporalSpriteRenderer {
    pub fn create(
        device: &wgpu::Device,
        atlas: &GpuTextureAtlas,
        width: u32,
        height: u32,
        views_per_batch: NonZeroU32,
        max_instances_per_view: NonZeroU32,
        in_flight_batches: NonZeroU32,
    ) -> Result<Self> {
        let identity = next_identity(&TEMPORAL_RENDERER_ID)?;
        validate_multiview_count(views_per_batch)?;
        validate_in_flight_batches(in_flight_batches)?;
        if !device
            .features()
            .contains(SpritePipeline::REQUIRED_FEATURES)
        {
            return Err(Error::Invalid(
                "GPU device lacks required multiview rendering support".to_owned(),
            ));
        }
        let max_instances = u64::from(views_per_batch.get())
            .checked_mul(u64::from(max_instances_per_view.get()))
            .ok_or(Error::ArithmeticOverflow)?;
        let instance_bytes = max_instances
            .checked_mul(std::mem::size_of::<SpriteInstance>() as u64)
            .ok_or(Error::ArithmeticOverflow)?;
        let max_storage_size = device.limits().max_storage_buffer_binding_size as u64;
        if instance_bytes > max_storage_size {
            return Err(Error::Invalid(
                "temporal sprite batch exceeds the GPU storage-buffer limit".to_owned(),
            ));
        }

        let target_format = PIXI_COLOR_FORMAT;
        let pipeline = SpritePipeline::create(device, target_format, views_per_batch)?;
        let blur_pipeline = SpriteBlurPipeline::create(device, target_format, views_per_batch)?;
        let slot_count =
            usize::try_from(in_flight_batches.get()).map_err(|_| Error::ArithmeticOverflow)?;
        validate_temporal_color_budget(width, height, views_per_batch, in_flight_batches)?;
        let blur_scratch = SpriteBlurScratch {
            ping: TemporalTarget::create(device, width, height, views_per_batch, target_format)?,
            pong: TemporalTarget::create(device, width, height, views_per_batch, target_format)?,
        };
        let mut slots = Vec::with_capacity(slot_count);
        for _ in 0..slot_count {
            let target =
                TemporalTarget::create(device, width, height, views_per_batch, target_format)?;
            let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("temporal sprite instances"),
                size: instance_bytes,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::STORAGE,
                mapped_at_creation: false,
            });
            let frame_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("temporal frame configuration"),
                size: std::mem::size_of::<FrameConfig>() as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::UNIFORM,
                mapped_at_creation: false,
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("temporal sprite bind group"),
                layout: &pipeline.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: instance_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: frame_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&atlas.view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::Sampler(&atlas.sampler),
                    },
                ],
            });
            let blur = SpriteBlurSlot::create(
                device,
                &blur_pipeline,
                &blur_scratch,
                &instance_buffer,
                &frame_buffer,
                max_instances_per_view.get(),
            )?;
            slots.push(TemporalBatchSlot {
                bind_group,
                instance_buffer,
                frame_buffer,
                target,
                blur,
            });
        }
        Ok(Self {
            pipeline,
            blur_pipeline,
            blur_scratch,
            slots,
            views_per_batch,
            max_instances_per_view: max_instances_per_view.get(),
            identity,
        })
    }

    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    pub(crate) fn vector_filter_resources(&self) -> (u64, &TemporalTarget, &TemporalTarget) {
        (
            self.identity,
            &self.blur_scratch.ping,
            &self.blur_scratch.pong,
        )
    }

    pub(crate) const fn identity(&self) -> u64 {
        self.identity
    }

    /// Expose immutable target metadata so output converters can be created
    /// once per ring slot before a submission borrows the renderer.
    pub fn target(&self, slot_index: usize) -> Result<&TemporalTarget> {
        self.slots
            .get(slot_index)
            .map(|slot| &slot.target)
            .ok_or_else(|| Error::Invalid(format!("invalid temporal batch slot {slot_index}")))
    }

    pub fn begin_submission<'a>(
        &'a mut self,
        device: &wgpu::Device,
    ) -> Result<TemporalSubmission<'a>> {
        self.begin_submission_at(device, 0)
    }

    pub fn begin_submission_at<'a>(
        &'a mut self,
        device: &wgpu::Device,
        first_slot: usize,
    ) -> Result<TemporalSubmission<'a>> {
        if first_slot >= self.slot_count() {
            return Err(Error::Invalid(format!(
                "temporal submission starts at invalid slot {first_slot}"
            )));
        }
        let submission_id = next_identity(&TEMPORAL_SUBMISSION_ID)?;
        Ok(TemporalSubmission {
            renderer: self,
            encoder: Some(
                device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("temporal frame submission"),
                }),
            ),
            first_slot,
            next_slot: first_slot,
            submission_id,
        })
    }

    fn prepare_batch_into_slot(
        &self,
        queue: &wgpu::Queue,
        slot_index: usize,
        batch: &TemporalSpriteBatch,
    ) -> Result<Range<u32>> {
        let slot = self
            .slots
            .get(slot_index)
            .ok_or_else(|| Error::Invalid(format!("invalid temporal batch slot {slot_index}")))?;
        // A run describes fixed z-order slots shared by every temporal view.
        // Callers use invisible placeholder instances when a node is absent in
        // one view so blend ordering never changes between timestamps.
        let active_views = batch.active_views.get();
        if active_views > self.views_per_batch.get() {
            return Err(Error::Invalid(
                "active temporal views exceed the render batch".to_owned(),
            ));
        }
        if batch.instances_per_view > self.max_instances_per_view {
            return Err(Error::Invalid(
                "temporal sprite count exceeds the configured capacity".to_owned(),
            ));
        }
        // The render pipeline always executes every statically configured
        // multiview layer. Inactive views still index the storage buffer before
        // the shader masks their output, so callers must provide padded slots
        // for all configured views.
        let expected_instances = (self.views_per_batch.get() as usize)
            .checked_mul(batch.instances_per_view as usize)
            .ok_or(Error::ArithmeticOverflow)?;
        if batch.instances.len() != expected_instances {
            return Err(Error::Invalid(format!(
                "temporal sprite batch has {} instances; expected {expected_instances}",
                batch.instances.len()
            )));
        }
        validate_draw_runs(&batch.draw_runs, batch.instances_per_view)?;
        let frame = FrameConfig {
            instances_per_view: batch.instances_per_view,
            active_views,
            output_size: [slot.target.width as f32, slot.target.height as f32],
        };
        if !batch.instances.is_empty() {
            queue.write_buffer(
                &slot.instance_buffer,
                0,
                bytemuck::cast_slice(&batch.instances),
            );
        }
        queue.write_buffer(&slot.frame_buffer, 0, bytemuck::bytes_of(&frame));
        Ok(0..active_views)
    }

    fn encode_runs_into_slot(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        slot_index: usize,
        target: Option<&wgpu::TextureView>,
        draw_runs: impl Iterator<Item = SpriteDrawRun>,
        load: wgpu::LoadOp<wgpu::Color>,
    ) -> Result<()> {
        let slot = self
            .slots
            .get(slot_index)
            .ok_or_else(|| Error::Invalid(format!("invalid temporal batch slot {slot_index}")))?;
        let target = target.unwrap_or(&slot.target.view);
        let mut draw_runs = draw_runs.peekable();
        if draw_runs.peek().is_none() {
            encode_empty_sprite_pass(encoder, target, load);
            return Ok(());
        }
        let mut load = load;
        while let Some(mut run) = draw_runs.next() {
            if run.has_blur_filter {
                self.encode_blurred_run(encoder, slot, target, &run, load)?;
            } else {
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
                    label: Some("temporal sprite frame batch"),
                    color_attachments: &[attachment],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });
                pass.set_bind_group(0, &slot.bind_group, &[]);
                loop {
                    pass.set_pipeline(self.pipeline.for_blend_mode(run.blend_mode));
                    pass.draw(0..6, run.instances);
                    if draw_runs.peek().is_some_and(|next| !next.has_blur_filter) {
                        run = draw_runs.next().expect("peeked sprite draw run");
                    } else {
                        break;
                    }
                }
            }
            load = wgpu::LoadOp::Load;
        }
        Ok(())
    }

    fn encode_blurred_run(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        slot: &TemporalBatchSlot,
        target: &wgpu::TextureView,
        run: &SpriteDrawRun,
        load: wgpu::LoadOp<wgpu::Color>,
    ) -> Result<()> {
        if run.instances.end != run.instances.start + 1 {
            return Err(Error::Invalid(
                "each filtered sprite must occupy an isolated draw run".to_owned(),
            ));
        }
        encode_sprite_run(
            encoder,
            &self.pipeline,
            slot,
            &self.blur_scratch.ping.view,
            run,
            wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
        );
        let horizontal_offset =
            blur_config_offset(run.instances.start, 1, self.blur_pipeline.config_stride)?;
        let vertical_offset =
            blur_config_offset(run.instances.start, 0, self.blur_pipeline.config_stride)?;
        let final_offset =
            blur_config_offset(run.instances.start, 2, self.blur_pipeline.config_stride)?;
        for (source, destination, offset, load) in [
            (
                &slot.blur.ping_blur_bind_group,
                &self.blur_scratch.pong.view,
                horizontal_offset,
                wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
            ),
            (
                &slot.blur.pong_blur_bind_group,
                &self.blur_scratch.ping.view,
                horizontal_offset,
                wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
            ),
            (
                &slot.blur.ping_blur_bind_group,
                &self.blur_scratch.pong.view,
                horizontal_offset,
                wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
            ),
            (
                &slot.blur.pong_blur_bind_group,
                &self.blur_scratch.ping.view,
                horizontal_offset,
                wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
            ),
            (
                &slot.blur.ping_blur_bind_group,
                &self.blur_scratch.pong.view,
                vertical_offset,
                wgpu::LoadOp::Load,
            ),
            (
                &slot.blur.pong_blur_bind_group,
                &self.blur_scratch.ping.view,
                vertical_offset,
                wgpu::LoadOp::Load,
            ),
            (
                &slot.blur.ping_blur_bind_group,
                &self.blur_scratch.pong.view,
                vertical_offset,
                wgpu::LoadOp::Load,
            ),
        ] {
            encode_sprite_blur_pass(
                encoder,
                &self.blur_pipeline.pipeline,
                source,
                destination,
                offset,
                load,
            );
        }
        encode_sprite_blur_pass(
            encoder,
            &self.blur_pipeline.final_pipeline,
            &slot.blur.pong_blur_bind_group,
            target,
            final_offset,
            load,
        );
        Ok(())
    }
}

fn validate_cached_target(cache: &TemporalTarget, target: &TemporalTarget) -> Result<()> {
    if cache.width != target.width
        || cache.height != target.height
        || cache.layers != target.layers
        || cache.format != target.format
    {
        return Err(Error::Invalid(
            "cached terrain target differs from the temporal scene target".to_owned(),
        ));
    }
    Ok(())
}

fn copy_temporal_target(
    encoder: &mut wgpu::CommandEncoder,
    source: &TemporalTarget,
    destination: &TemporalTarget,
) {
    encoder.copy_texture_to_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &source.texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyTextureInfo {
            texture: &destination.texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::Extent3d {
            width: source.width,
            height: source.height,
            depth_or_array_layers: source.layers.get(),
        },
    );
}

impl TemporalSubmission<'_> {
    pub fn lease_batch(&mut self) -> Result<TemporalBatchLease> {
        let slot_index = self.next_slot;
        if slot_index >= self.renderer.slot_count() {
            return Err(Error::Invalid(format!(
                "temporal submission exceeds its {} in-flight batch slots",
                self.renderer.slot_count()
            )));
        }
        self.next_slot += 1;
        Ok(TemporalBatchLease {
            submission_id: self.submission_id,
            slot_index,
        })
    }

    pub fn prepare_leased_batch(
        &mut self,
        queue: &wgpu::Queue,
        lease: TemporalBatchLease,
        batch: &TemporalSpriteBatch,
    ) -> Result<EncodedTemporalBatch> {
        self.validate_lease(&lease)?;
        let active_layers =
            self.renderer
                .prepare_batch_into_slot(queue, lease.slot_index, batch)?;
        Ok(EncodedTemporalBatch {
            submission_id: lease.submission_id,
            slot_index: lease.slot_index,
            active_layers,
            instances_per_view: batch.instances_per_view,
            draw_runs: batch.draw_runs.clone(),
            slot_activations: batch.slot_activations.clone(),
        })
    }

    /// Draw one metadata layer from an already uploaded sprite batch. Terrain
    /// and filter passes can be recorded between calls while preserving exact
    /// @pixi/layers order.
    pub fn encode_sprite_layer(
        &mut self,
        batch: &EncodedTemporalBatch,
        layer_order: u32,
        load: wgpu::LoadOp<wgpu::Color>,
    ) -> Result<()> {
        self.validate_encoded_batch(batch)?;
        validate_draw_runs(&batch.draw_runs, batch.instances_per_view)?;
        let selected = batch
            .draw_runs
            .iter()
            .filter(|run| run.layer_order == layer_order)
            .cloned();
        let encoder = self
            .encoder
            .as_mut()
            .expect("submission retains its encoder until submit");
        self.renderer
            .encode_runs_into_slot(encoder, batch.slot_index, None, selected, load)
    }

    /// Draw one sprite activation so heterogeneous vector/sprite display
    /// order can be replayed without collapsing intervening drawables.
    pub fn encode_sprite_activation(
        &mut self,
        batch: &EncodedTemporalBatch,
        activation_order: u32,
        load: wgpu::LoadOp<wgpu::Color>,
    ) -> Result<()> {
        self.encode_sprite_activations(batch, &[activation_order], load)
    }

    pub fn encode_sprite_activations(
        &mut self,
        batch: &EncodedTemporalBatch,
        activation_orders: &[u32],
        load: wgpu::LoadOp<wgpu::Color>,
    ) -> Result<()> {
        self.validate_encoded_batch(batch)?;
        validate_draw_runs(&batch.draw_runs, batch.instances_per_view)?;
        let runs = activation_orders
            .iter()
            .map(|activation_order| sprite_activation_run(batch, *activation_order))
            .collect::<Result<Vec<_>>>()?;
        let encoder = self
            .encoder
            .as_mut()
            .expect("submission retains its encoder until submit");
        self.renderer
            .encode_runs_into_slot(encoder, batch.slot_index, None, runs.into_iter(), load)
    }

    /// Draw one sprite layer into a compatible external temporal target, such
    /// as the filtered lighting-layer intermediate.
    fn encode_sprite_layer_to(
        &mut self,
        batch: &EncodedTemporalBatch,
        layer_order: u32,
        target: &TemporalTarget,
        load: wgpu::LoadOp<wgpu::Color>,
    ) -> Result<()> {
        self.validate_encoded_batch(batch)?;
        let main_target = &self.renderer.slots[batch.slot_index].target;
        if target.width != main_target.width
            || target.height != main_target.height
            || target.layers != main_target.layers
            || target.format != main_target.format
        {
            return Err(Error::Invalid(
                "external sprite target differs from the temporal scene target".to_owned(),
            ));
        }
        validate_draw_runs(&batch.draw_runs, batch.instances_per_view)?;
        let selected = batch
            .draw_runs
            .iter()
            .filter(|run| run.layer_order == layer_order)
            .cloned();
        let encoder = self
            .encoder
            .as_mut()
            .expect("submission retains its encoder until submit");
        self.renderer.encode_runs_into_slot(
            encoder,
            batch.slot_index,
            Some(&target.view),
            selected,
            load,
        )
    }

    /// Draw a filtered sprite layer into the compositor slot belonging to this
    /// exact temporal batch.
    pub fn encode_sprite_layer_to_lighting(
        &mut self,
        compositor: &crate::TemporalLayerCompositor,
        batch: &EncodedTemporalBatch,
        layer_order: u32,
        load: wgpu::LoadOp<wgpu::Color>,
    ) -> Result<()> {
        let target = compositor.lighting_target(batch)?;
        self.encode_sprite_layer_to(batch, layer_order, target, load)
    }

    /// Multiply the matching compositor lighting slot onto this batch's own
    /// scene target. Neither source nor destination slot is caller-selectable.
    pub fn encode_lighting_composite(
        &mut self,
        compositor: &crate::TemporalLayerCompositor,
        batch: &EncodedTemporalBatch,
        load: wgpu::LoadOp<wgpu::Color>,
    ) -> Result<()> {
        self.validate_encoded_batch(batch)?;
        let target = &self.renderer.slots[batch.slot_index].target;
        let encoder = self
            .encoder
            .as_mut()
            .expect("submission retains its encoder until submit");
        compositor.encode_lighting_composite_into(encoder, batch.slot_index, target, load)
    }

    /// Draw one compiled terrain phase into a leased scene target before the
    /// matching sprite/vector batch is prepared. Keeping the lease in this
    /// owning submission prevents a terrain phase from landing in another
    /// in-flight target slot.
    pub fn encode_terrain_phase(
        &mut self,
        queue: &wgpu::Queue,
        lease: &TemporalBatchLease,
        uploads: &mut crate::TerrainCommandUploads,
        parameters: LeasedTerrainPhase<'_>,
    ) -> Result<()> {
        let LeasedTerrainPhase {
            pipeline,
            bindings,
            batch,
            load,
        } = parameters;
        self.validate_lease(lease)?;
        let target = &self.renderer.slots[lease.slot_index].target;
        if batch.frame.output_size != [target.width as f32, target.height as f32]
            || batch.frame.active_views > target.layers.get()
        {
            return Err(Error::Invalid(
                "terrain phase dimensions differ from the leased temporal target".to_owned(),
            ));
        }
        let encoder = self
            .encoder
            .as_mut()
            .expect("submission retains its encoder until submit");
        pipeline.encode(
            queue,
            encoder,
            uploads,
            crate::TerrainEncodePass {
                target: &target.view,
                bindings,
                instances: &batch.instances,
                frame: batch.frame,
                runs: &batch.runs,
                load,
            },
        )
    }

    /// Encode one complete terrain + object microbatch into a single leased
    /// target slot, including the retained lighting intermediate and effects.
    pub fn encode_render_batch(
        &mut self,
        queue: &wgpu::Queue,
        uploads: &mut crate::TerrainCommandUploads,
        parameters: TemporalRenderBatch<'_>,
    ) -> Result<EncodedTemporalBatch> {
        let TemporalRenderBatch {
            vector_pipeline,
            terrain_pipeline,
            terrain_bindings,
            compositor,
            terrain,
            scene,
            clear_color,
            terrain_cache,
        } = parameters;
        if !vector_pipeline.is_compatible_renderer(self.renderer)
            || scene.sprites.active_views != scene.vectors.active_views
        {
            return Err(Error::Invalid(
                "combined terrain scene has incompatible renderer or active views".to_owned(),
            ));
        }
        let resident_lighting = terrain_cache.and_then(|cache| cache.lighting);
        if resident_lighting.is_some() && terrain.lighting.is_some() {
            return Err(Error::Invalid(
                "resident and dynamic terrain lighting cannot be supplied together".to_owned(),
            ));
        }
        let has_lighting = resident_lighting.is_some() || terrain.lighting.is_some();
        let lighting_composite_supported =
            matches!(
                (has_lighting, terrain.lighting_composite),
                (true, Some(composite))
                    if composite.alpha == 1.0
                        && composite.blend_mode == SpriteBlendMode::Multiply
            ) || matches!((has_lighting, terrain.lighting_composite), (false, None));
        if !lighting_composite_supported {
            return Err(Error::Invalid(
                "terrain lighting phase and composite are inconsistent".to_owned(),
            ));
        }
        let phases = [
            terrain.terrain.as_ref(),
            terrain.wall_graffiti.as_ref(),
            terrain.lighting.as_ref(),
            terrain.effects.as_ref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        if uploads.remaining_passes()
            < u32::try_from(phases.len()).map_err(|_| Error::ArithmeticOverflow)?
        {
            return Err(Error::Invalid(
                "terrain upload arena cannot hold the complete render batch".to_owned(),
            ));
        }
        let target = &self
            .renderer
            .slots
            .first()
            .expect("temporal renderer has at least one ring slot")
            .target;
        let expected_active_views = scene.sprites.active_views.get();
        for phase in phases {
            let expected_instances = phase
                .frame
                .instances_per_view
                .checked_mul(target.layers.get())
                .ok_or(Error::ArithmeticOverflow)?;
            if phase.frame.active_views != expected_active_views
                || phase.frame.output_size != [target.width as f32, target.height as f32]
                || usize::try_from(expected_instances).ok() != Some(phase.instances.len())
                || expected_instances > terrain_bindings.capacity
            {
                return Err(Error::Invalid(
                    "terrain phase does not match the complete temporal scene batch".to_owned(),
                ));
            }
        }
        let lease = self.lease_batch()?;
        let mut scene_load = if let Some(cache) = terrain_cache {
            let target = &self.renderer.slots[lease.slot_index].target;
            validate_cached_target(cache.prefix, target)?;
            let encoder = self
                .encoder
                .as_mut()
                .expect("submission retains its encoder until submit");
            copy_temporal_target(encoder, cache.prefix, target);
            wgpu::LoadOp::Load
        } else {
            wgpu::LoadOp::Clear(clear_color)
        };
        for phase in [&terrain.terrain, &terrain.wall_graffiti]
            .into_iter()
            .flatten()
        {
            self.encode_terrain_phase(
                queue,
                &lease,
                uploads,
                LeasedTerrainPhase {
                    pipeline: terrain_pipeline,
                    bindings: terrain_bindings,
                    batch: phase,
                    load: scene_load,
                },
            )?;
            scene_load = wgpu::LoadOp::Load;
        }
        let encoded =
            self.encode_leased_scene_batch(queue, vector_pipeline, lease, scene, scene_load)?;
        if let Some(source) = resident_lighting {
            let target = &self.renderer.slots[encoded.slot_index].target;
            let encoder = self
                .encoder
                .as_mut()
                .expect("submission retains its encoder until submit");
            compositor.encode_resident_lighting_composite_into(
                encoder,
                source,
                target,
                wgpu::LoadOp::Load,
            )?;
        } else if let Some(lighting) = &terrain.lighting {
            let target = compositor.lighting_target(&encoded)?;
            if lighting.frame.output_size != [target.width as f32, target.height as f32]
                || lighting.frame.active_views > target.layers.get()
            {
                return Err(Error::Invalid(
                    "terrain lighting dimensions differ from the compositor target".to_owned(),
                ));
            }
            let encoder = self
                .encoder
                .as_mut()
                .expect("submission retains its encoder until submit");
            terrain_pipeline.encode(
                queue,
                encoder,
                uploads,
                crate::TerrainEncodePass {
                    target: &target.view,
                    bindings: terrain_bindings,
                    instances: &lighting.instances,
                    frame: lighting.frame,
                    runs: &lighting.runs,
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                },
            )?;
        }
        if has_lighting && resident_lighting.is_none() {
            self.encode_lighting_composite(compositor, &encoded, wgpu::LoadOp::Load)?;
        }
        if let Some(effects) = &terrain.effects {
            let (target, encoder) = self.target_and_encoder(&encoded)?;
            terrain_pipeline.encode(
                queue,
                encoder,
                uploads,
                crate::TerrainEncodePass {
                    target: &target.view,
                    bindings: terrain_bindings,
                    instances: &effects.instances,
                    frame: effects.frame,
                    runs: &effects.runs,
                    load: wgpu::LoadOp::Load,
                },
            )?;
        }
        Ok(encoded)
    }

    /// Append conversion and copy for this batch to the owned command encoder.
    /// The returned token can only be completed by this exact submission.
    pub fn encode_nv12_readback<'r>(
        &mut self,
        batch: &EncodedTemporalBatch,
        converter: &crate::Nv12BatchConverter,
        readback: &'r mut crate::Nv12ReadbackBuffer,
    ) -> Result<PendingTemporalReadback<'r>> {
        self.validate_encoded_batch(batch)?;
        let target = &self.renderer.slots[batch.slot_index].target;
        if converter.source_identity() != target.identity {
            return Err(Error::Invalid(
                "NV12 converter belongs to another temporal target".to_owned(),
            ));
        }
        let encoder = self
            .encoder
            .as_mut()
            .expect("submission retains its encoder until submit");
        converter.encode(encoder);
        let copy = converter.copy_to_readback(encoder, readback, batch.active_layers.end)?;
        Ok(PendingTemporalReadback {
            submission_id: self.submission_id,
            copy,
        })
    }

    /// Convert a completed temporal batch to GPU-resident NV12 without
    /// scheduling a host readback. Hardware encoders and throughput probes use
    /// this boundary so conversion can remain in the same command submission
    /// as rendering.
    pub fn encode_nv12(
        &mut self,
        batch: &EncodedTemporalBatch,
        converter: &crate::Nv12BatchConverter,
    ) -> Result<()> {
        self.validate_encoded_batch(batch)?;
        let target = &self.renderer.slots[batch.slot_index].target;
        if converter.source_identity() != target.identity {
            return Err(Error::Invalid(
                "NV12 converter belongs to another temporal target".to_owned(),
            ));
        }
        let encoder = self
            .encoder
            .as_mut()
            .expect("submission retains its encoder until submit");
        converter.encode(encoder);
        Ok(())
    }

    /// Convert each active temporal layer into an independent packed R8 NV12
    /// target. The destination order is the temporal frame order, allowing a
    /// ring of exportable Vulkan images to be handed directly to CUDA/NVENC.
    pub fn encode_packed_nv12(
        &mut self,
        batch: &EncodedTemporalBatch,
        converter: &crate::PackedNv12Converter,
        destinations: &[&wgpu::TextureView],
    ) -> Result<()> {
        self.validate_encoded_batch(batch)?;
        let target = &self.renderer.slots[batch.slot_index].target;
        if converter.source_identity() != target.identity {
            return Err(Error::Invalid(
                "packed NV12 converter belongs to another temporal target".to_owned(),
            ));
        }
        let encoder = self
            .encoder
            .as_mut()
            .expect("submission retains its encoder until submit");
        converter.encode(encoder, destinations, batch.active_layers.end)
    }

    pub fn encode_batch(
        &mut self,
        queue: &wgpu::Queue,
        batch: &TemporalSpriteBatch,
    ) -> Result<EncodedTemporalBatch> {
        let lease = self.lease_batch()?;
        let encoded = self.prepare_leased_batch(queue, lease, batch)?;
        let encoder = self
            .encoder
            .as_mut()
            .expect("submission retains its encoder until submit");
        self.renderer.encode_runs_into_slot(
            encoder,
            encoded.slot_index,
            None,
            batch.draw_runs.iter().cloned(),
            wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
        )?;
        Ok(encoded)
    }

    /// Upload and encode one heterogeneous sprite/vector scene batch in its
    /// exact display order. Both drawable types share this submission and
    /// temporal target; each activation remains independently ordered.
    pub fn encode_scene_batch(
        &mut self,
        queue: &wgpu::Queue,
        vector_pipeline: &crate::VectorPipeline,
        batch: &crate::TemporalSceneBatch,
    ) -> Result<EncodedTemporalBatch> {
        let lease = self.lease_batch()?;
        self.encode_leased_scene_batch(
            queue,
            vector_pipeline,
            lease,
            batch,
            wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
        )
    }

    /// Encode a heterogeneous scene into a caller-reserved slot after terrain
    /// phases have already targeted that exact lease.
    pub fn encode_leased_scene_batch(
        &mut self,
        queue: &wgpu::Queue,
        vector_pipeline: &crate::VectorPipeline,
        lease: TemporalBatchLease,
        batch: &crate::TemporalSceneBatch,
        initial_load: wgpu::LoadOp<wgpu::Color>,
    ) -> Result<EncodedTemporalBatch> {
        self.validate_lease(&lease)?;
        if !vector_pipeline.is_compatible_renderer(self.renderer) {
            return Err(Error::Invalid(
                "vector pipeline belongs to another temporal renderer".to_owned(),
            ));
        }
        if batch.sprites.active_views != batch.vectors.active_views {
            return Err(Error::Invalid(
                "heterogeneous scene batch has inconsistent active views".to_owned(),
            ));
        }
        let mut seen = BTreeSet::new();
        let mut display_sprites = BTreeSet::new();
        let mut display_vectors = BTreeSet::new();
        for entry in &batch.display_order {
            if !seen.insert((entry.activation_order, entry.kind)) {
                return Err(Error::Invalid(
                    "heterogeneous scene display order repeats a drawable identity".to_owned(),
                ));
            }
            match entry.kind {
                crate::SceneDrawableKind::Sprite => {
                    display_sprites.insert(entry.activation_order);
                }
                crate::SceneDrawableKind::Vector => {
                    display_vectors.insert(entry.activation_order);
                }
            }
        }
        if display_sprites != batch.sprites.slot_activations.iter().copied().collect()
            || display_vectors != batch.vectors.slot_activations.iter().copied().collect()
        {
            return Err(Error::Invalid(
                "heterogeneous scene display order does not cover its packed drawables".to_owned(),
            ));
        }
        let encoded = self.prepare_leased_batch(queue, lease, &batch.sprites)?;
        let target = &self.renderer.slots[encoded.slot_index].target;
        if vector_pipeline.output_size() != [target.width, target.height] {
            return Err(Error::Invalid(
                "vector pipeline dimensions differ from the temporal scene target".to_owned(),
            ));
        }
        let vectors = vector_pipeline.prepare_batch(queue, encoded.slot_index, &batch.vectors)?;
        {
            let encoder = self
                .encoder
                .as_mut()
                .expect("submission retains its encoder until submit");
            self.renderer.encode_runs_into_slot(
                encoder,
                encoded.slot_index,
                None,
                std::iter::empty(),
                initial_load,
            )?;
        }
        let mut start = 0;
        while start < batch.display_order.len() {
            let kind = batch.display_order[start].kind;
            let mut end = start + 1;
            while end < batch.display_order.len() && batch.display_order[end].kind == kind {
                end += 1;
            }
            let activation_orders = batch.display_order[start..end]
                .iter()
                .map(|entry| entry.activation_order)
                .collect::<Vec<_>>();
            match kind {
                crate::SceneDrawableKind::Sprite => {
                    self.encode_sprite_activations(
                        &encoded,
                        &activation_orders,
                        wgpu::LoadOp::Load,
                    )?;
                }
                crate::SceneDrawableKind::Vector => {
                    let (target, encoder) = self.target_and_encoder(&encoded)?;
                    vector_pipeline.encode_prepared_activations(
                        encoder,
                        &target.view,
                        &vectors,
                        &activation_orders,
                        wgpu::LoadOp::Load,
                    )?;
                }
            }
            start = end;
        }
        Ok(encoded)
    }

    /// Borrow a reserved target before sprite drawing so terrain and
    /// compositor passes can share the same temporal texture array.
    pub fn leased_target_and_encoder(
        &mut self,
        lease: &TemporalBatchLease,
    ) -> Result<(&TemporalTarget, &mut wgpu::CommandEncoder)> {
        self.validate_lease(lease)?;
        let target = &self.renderer.slots[lease.slot_index].target;
        let encoder = self
            .encoder
            .as_mut()
            .expect("submission retains its encoder until submit");
        Ok((target, encoder))
    }

    /// Borrow an encoded target together with the submission encoder so
    /// conversion/copy passes can be appended before queue submission.
    pub fn target_and_encoder(
        &mut self,
        batch: &EncodedTemporalBatch,
    ) -> Result<(&TemporalTarget, &mut wgpu::CommandEncoder)> {
        self.validate_encoded_batch(batch)?;
        let target = &self.renderer.slots[batch.slot_index].target;
        let encoder = self
            .encoder
            .as_mut()
            .expect("submission retains its encoder until submit");
        Ok((target, encoder))
    }

    fn validate_lease(&self, lease: &TemporalBatchLease) -> Result<()> {
        if lease.submission_id != self.submission_id {
            return Err(Error::Invalid(
                "temporal batch lease belongs to another submission".to_owned(),
            ));
        }
        if lease.slot_index < self.first_slot || lease.slot_index >= self.next_slot {
            return Err(Error::Invalid(format!(
                "temporal batch slot {} is not leased in this submission",
                lease.slot_index
            )));
        }
        Ok(())
    }

    fn validate_encoded_batch(&self, batch: &EncodedTemporalBatch) -> Result<()> {
        if batch.submission_id != self.submission_id {
            return Err(Error::Invalid(
                "temporal batch handle belongs to another submission".to_owned(),
            ));
        }
        if batch.slot_index < self.first_slot || batch.slot_index >= self.next_slot {
            return Err(Error::Invalid(format!(
                "temporal batch slot {} has not been leased in this submission",
                batch.slot_index
            )));
        }
        Ok(())
    }

    pub fn submit(mut self, queue: &wgpu::Queue) -> wgpu::SubmissionIndex {
        let encoder = self
            .encoder
            .take()
            .expect("temporal submission can only be submitted once");
        queue.submit(Some(encoder.finish()))
    }

    pub fn submit_and_read_nv12(
        mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pending: PendingTemporalReadback<'_>,
    ) -> Result<Vec<Vec<u8>>> {
        if pending.submission_id != self.submission_id {
            return Err(Error::Invalid(
                "NV12 readback belongs to another temporal submission".to_owned(),
            ));
        }
        let encoder = self
            .encoder
            .take()
            .expect("temporal submission can only be submitted once");
        let submission = queue.submit(Some(encoder.finish()));
        pending.copy.read(device, submission)
    }

    /// Submit this temporal batch and visit each tightly packed NV12 frame in
    /// temporal-layer order. Frames borrow mapped readback memory when its
    /// layout is already contiguous; padded rows share one reusable scratch
    /// buffer. The mapped GPU buffer is always unmapped before this returns.
    pub fn submit_and_visit_nv12<F>(
        mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pending: PendingTemporalReadback<'_>,
        visitor: F,
    ) -> Result<()>
    where
        F: FnMut(&[u8]) -> Result<()>,
    {
        if pending.submission_id != self.submission_id {
            return Err(Error::Invalid(
                "NV12 readback belongs to another temporal submission".to_owned(),
            ));
        }
        let encoder = self
            .encoder
            .take()
            .expect("temporal submission can only be submitted once");
        let submission = queue.submit(Some(encoder.finish()));
        pending.copy.visit(device, submission, visitor)
    }
}

fn validate_multiview_count(views_per_batch: NonZeroU32) -> Result<()> {
    if !(SpritePipeline::MIN_VIEWS_PER_BATCH..=SpritePipeline::MAX_VIEWS_PER_BATCH)
        .contains(&views_per_batch.get())
    {
        return Err(Error::Invalid(format!(
            "wgpu temporal multiview batches require {} to {} views",
            SpritePipeline::MIN_VIEWS_PER_BATCH,
            SpritePipeline::MAX_VIEWS_PER_BATCH
        )));
    }
    Ok(())
}

fn validate_in_flight_batches(in_flight_batches: NonZeroU32) -> Result<()> {
    if in_flight_batches.get() > SpritePipeline::MAX_IN_FLIGHT_BATCHES {
        return Err(Error::Invalid(format!(
            "temporal renderer supports at most {} in-flight batches",
            SpritePipeline::MAX_IN_FLIGHT_BATCHES
        )));
    }
    Ok(())
}

fn validate_temporal_color_budget(
    width: u32,
    height: u32,
    views_per_batch: NonZeroU32,
    in_flight_batches: NonZeroU32,
) -> Result<()> {
    let texture_count = u64::from(in_flight_batches.get())
        .checked_add(2)
        .ok_or(Error::ArithmeticOverflow)?;
    let bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|value| value.checked_mul(u64::from(views_per_batch.get())))
        .and_then(|value| value.checked_mul(4))
        .and_then(|value| value.checked_mul(texture_count))
        .ok_or(Error::ArithmeticOverflow)?;
    if bytes > SpritePipeline::MAX_TEMPORAL_COLOR_BYTES {
        return Err(Error::Invalid(format!(
            "temporal color targets require {bytes} bytes; limit is {}",
            SpritePipeline::MAX_TEMPORAL_COLOR_BYTES
        )));
    }
    Ok(())
}

fn sprite_activation_run(
    batch: &EncodedTemporalBatch,
    activation_order: u32,
) -> Result<SpriteDrawRun> {
    let slot = batch
        .slot_activations
        .iter()
        .position(|activation| *activation == activation_order)
        .ok_or_else(|| {
            Error::Invalid(format!(
                "temporal sprite batch lacks activation {activation_order}"
            ))
        })?;
    let slot = u32::try_from(slot).map_err(|_| Error::ArithmeticOverflow)?;
    let containing = batch
        .draw_runs
        .iter()
        .find(|run| run.instances.contains(&slot))
        .expect("validated draw runs cover every sprite slot");
    Ok(SpriteDrawRun {
        layer_order: containing.layer_order,
        blend_mode: containing.blend_mode,
        has_blur_filter: containing.has_blur_filter,
        instances: slot..slot + 1,
    })
}

fn validate_draw_runs(draw_runs: &[SpriteDrawRun], instances_per_view: u32) -> Result<()> {
    let mut expected_start = 0;
    for run in draw_runs {
        if run.instances.start != expected_start
            || run.instances.end <= run.instances.start
            || run.instances.end > instances_per_view
            || (run.has_blur_filter && run.instances.end != run.instances.start + 1)
        {
            return Err(Error::Invalid(
                "sprite blend runs must cover the instance range exactly once in order".to_owned(),
            ));
        }
        expected_start = run.instances.end;
    }
    if expected_start != instances_per_view {
        return Err(Error::Invalid(
            "sprite blend runs do not cover every instance".to_owned(),
        ));
    }
    Ok(())
}

fn encode_empty_sprite_pass(
    encoder: &mut wgpu::CommandEncoder,
    target: &wgpu::TextureView,
    load: wgpu::LoadOp<wgpu::Color>,
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
    let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("empty temporal sprite frame batch"),
        color_attachments: &[attachment],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    });
}

fn encode_sprite_run(
    encoder: &mut wgpu::CommandEncoder,
    pipeline: &SpritePipeline,
    slot: &TemporalBatchSlot,
    target: &wgpu::TextureView,
    run: &SpriteDrawRun,
    load: wgpu::LoadOp<wgpu::Color>,
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
        label: Some("temporal sprite frame batch"),
        color_attachments: &[attachment],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    });
    pass.set_bind_group(0, &slot.bind_group, &[]);
    pass.set_pipeline(pipeline.for_blend_mode(run.blend_mode));
    pass.draw(0..6, run.instances.clone());
}

fn blur_config_offset(instance: u32, pass_kind: u32, stride: u32) -> Result<u32> {
    if pass_kind >= 3 {
        return Err(Error::Invalid(
            "sprite blur pass kind is out of range".to_owned(),
        ));
    }
    instance
        .checked_mul(3)
        .and_then(|index| index.checked_add(pass_kind))
        .and_then(|index| index.checked_mul(stride))
        .ok_or(Error::ArithmeticOverflow)
}

fn encode_sprite_blur_pass(
    encoder: &mut wgpu::CommandEncoder,
    pipeline: &wgpu::RenderPipeline,
    bind_group: &wgpu::BindGroup,
    target: &wgpu::TextureView,
    dynamic_offset: u32,
    load: wgpu::LoadOp<wgpu::Color>,
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
        label: Some("temporal sprite blur pass"),
        color_attachments: &[attachment],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    });
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, bind_group, &[dynamic_offset]);
    pass.draw(0..3, 0..1);
}

fn create_sprite_pipeline(
    device: &wgpu::Device,
    pipeline_layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    target_format: wgpu::TextureFormat,
    views_per_batch: NonZeroU32,
    label: &'static str,
    blend: wgpu::BlendState,
) -> wgpu::RenderPipeline {
    let target = Some(wgpu::ColorTargetState {
        format: target_format,
        blend: Some(blend),
        write_mask: wgpu::ColorWrites::ALL,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(pipeline_layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vertex_main"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fragment_main"),
            compilation_options: Default::default(),
            targets: &[target],
        }),
        multiview: Some(views_per_batch),
        cache: None,
    })
}

pub(crate) fn additive_blend() -> wgpu::BlendState {
    let component = wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::One,
        operation: wgpu::BlendOperation::Add,
    };
    wgpu::BlendState {
        color: component,
        alpha: component,
    }
}

pub(crate) fn multiply_blend() -> wgpu::BlendState {
    wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::Dst,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent::OVER,
    }
}

pub(crate) fn screen_blend() -> wgpu::BlendState {
    wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrc,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent::OVER,
    }
}

pub fn validate_sprite_shader() -> Result<()> {
    for (label, source) in [
        ("temporal sprite", SPRITE_SHADER),
        ("temporal sprite blur", SPRITE_BLUR_SHADER),
    ] {
        let module = naga::front::wgsl::parse_str(source)
            .map_err(|error| Error::Invalid(format!("{label} WGSL is invalid: {error}")))?;
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .map_err(|error| Error::Invalid(format!("{label} WGSL is unsupported: {error:#?}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::num::NonZeroU32;

    use crate::{AtlasEntry, TextureAtlas, TextureAtlasPage};

    use super::{
        FrameConfig, PIXI_COLOR_FORMAT, SpriteBlendMode, SpriteDrawRun, SpriteInstance,
        SpritePipeline, additive_blend, atlas_mip_level_count, blur_config_offset,
        build_atlas_mip_levels, multiply_blend, screen_blend, validate_draw_runs,
        validate_in_flight_batches, validate_multiview_count, validate_sprite_shader,
        validate_temporal_color_budget,
    };

    #[test]
    fn temporal_multiview_shader_validates_and_host_layouts_match_wgsl() {
        validate_sprite_shader().unwrap();
        assert_eq!(std::mem::size_of::<SpriteInstance>(), 96);
        assert_eq!(std::mem::align_of::<SpriteInstance>(), 4);
        assert_eq!(std::mem::size_of::<FrameConfig>(), 16);
        assert_eq!(PIXI_COLOR_FORMAT, wgpu::TextureFormat::Rgba8Unorm);
    }

    #[test]
    fn rejects_backend_invalid_multiview_counts() {
        for valid in SpritePipeline::MIN_VIEWS_PER_BATCH..=SpritePipeline::MAX_VIEWS_PER_BATCH {
            validate_multiview_count(NonZeroU32::new(valid).unwrap()).unwrap();
        }
        assert!(validate_multiview_count(NonZeroU32::new(1).unwrap()).is_err());
        assert!(
            validate_multiview_count(
                NonZeroU32::new(SpritePipeline::MAX_VIEWS_PER_BATCH + 1).unwrap()
            )
            .is_err()
        );
        for valid in 1..=SpritePipeline::MAX_IN_FLIGHT_BATCHES {
            validate_in_flight_batches(NonZeroU32::new(valid).unwrap()).unwrap();
        }
        assert!(
            validate_in_flight_batches(
                NonZeroU32::new(SpritePipeline::MAX_IN_FLIGHT_BATCHES + 1).unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn bounds_shared_temporal_color_targets_and_blur_offsets() {
        let views = NonZeroU32::new(SpritePipeline::MAX_VIEWS_PER_BATCH).unwrap();
        let slots = NonZeroU32::new(3).unwrap();
        validate_temporal_color_budget(1920, 1080, views, slots).unwrap();
        assert!(validate_temporal_color_budget(3840, 2160, views, slots).is_err());

        let stride = 256;
        assert_eq!(blur_config_offset(0, 0, stride).unwrap(), 0);
        assert_eq!(blur_config_offset(0, 1, stride).unwrap(), 256);
        assert_eq!(blur_config_offset(0, 2, stride).unwrap(), 512);
        assert_eq!(blur_config_offset(1, 0, stride).unwrap(), 768);
        assert!(blur_config_offset(0, 3, stride).is_err());
    }

    #[test]
    fn blend_runs_are_ordered_complete_and_match_pixis_webgl_factors() {
        let runs = [
            SpriteDrawRun {
                layer_order: 0,
                blend_mode: SpriteBlendMode::Normal,
                has_blur_filter: false,
                instances: 0..2,
            },
            SpriteDrawRun {
                layer_order: 0,
                blend_mode: SpriteBlendMode::Add,
                has_blur_filter: false,
                instances: 2..4,
            },
            SpriteDrawRun {
                layer_order: 1,
                blend_mode: SpriteBlendMode::Multiply,
                has_blur_filter: false,
                instances: 4..5,
            },
            SpriteDrawRun {
                layer_order: 1,
                blend_mode: SpriteBlendMode::Screen,
                has_blur_filter: false,
                instances: 5..6,
            },
        ];
        validate_draw_runs(&runs, 6).unwrap();
        assert!(validate_draw_runs(&runs[..2], 6).is_err());
        assert!(
            validate_draw_runs(
                &[SpriteDrawRun {
                    layer_order: 0,
                    blend_mode: SpriteBlendMode::Normal,
                    has_blur_filter: false,
                    instances: 1..6,
                }],
                6
            )
            .is_err()
        );
        assert!(
            validate_draw_runs(
                &[SpriteDrawRun {
                    layer_order: 0,
                    blend_mode: SpriteBlendMode::Add,
                    has_blur_filter: true,
                    instances: 0..2,
                }],
                2,
            )
            .is_err()
        );

        let add = additive_blend();
        assert_eq!(add.color.src_factor, wgpu::BlendFactor::One);
        assert_eq!(add.color.dst_factor, wgpu::BlendFactor::One);
        assert_eq!(add.alpha, add.color);
        let multiply = multiply_blend();
        assert_eq!(multiply.color.src_factor, wgpu::BlendFactor::Dst);
        assert_eq!(
            multiply.color.dst_factor,
            wgpu::BlendFactor::OneMinusSrcAlpha
        );
        assert_eq!(multiply.alpha, wgpu::BlendComponent::OVER);
        let screen = screen_blend();
        assert_eq!(screen.color.src_factor, wgpu::BlendFactor::One);
        assert_eq!(screen.color.dst_factor, wgpu::BlendFactor::OneMinusSrc);
        assert_eq!(screen.alpha, wgpu::BlendComponent::OVER);
    }

    #[test]
    fn builds_atlas_safe_box_filtered_mips_with_wrapped_entry_gutters() {
        let mut base = vec![0; 8 * 4 * 4];
        let red = ((2 * 8 + 2) * 4) as usize;
        base[red..red + 4].copy_from_slice(&[200, 0, 0, 255]);
        let blue = red + 4;
        base[blue..blue + 4].copy_from_slice(&[0, 0, 100, 255]);
        let atlas = TextureAtlas {
            entries: BTreeMap::from([(
                "tile".to_owned(),
                AtlasEntry {
                    page: 0,
                    x: 2,
                    y: 2,
                    width: 2,
                    height: 1,
                    logical_width: 2.0,
                    logical_height: 1.0,
                    u_min: 0.25,
                    v_min: 0.5,
                    u_max: 0.5,
                    v_max: 0.75,
                },
            )]),
            pages: vec![TextureAtlasPage {
                width: 8,
                height: 4,
                rgba: base,
            }],
            padding: 2,
        };
        assert_eq!(atlas_mip_level_count(&atlas, 8, 4), 2);
        let mip = build_atlas_mip_levels(&atlas, 2).unwrap();
        assert_eq!(mip[0][0].len(), 4 * 2 * 4);
        for x in 0..3 {
            let offset = ((4 + x) * 4) as usize;
            assert_eq!(&mip[0][0][offset..offset + 4], &[100, 0, 50, 255]);
        }
    }
}
