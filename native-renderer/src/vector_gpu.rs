use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::num::{NonZeroU32, NonZeroU64};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};

use bytemuck::{Pod, Zeroable};

use crate::{
    Error, FrameConfig, PIXI_COLOR_FORMAT, PreparedVector, Result, SpriteBlendMode, SpritePipeline,
    TemporalSpriteRenderer, VectorGeometryId, VectorMesh, gpu,
};

pub const MAX_TEMPORAL_VECTOR_VERTICES: usize = 4_194_304;
pub const MAX_TEMPORAL_VECTOR_INSTANCES: usize = 1_048_576;
pub const MAX_VECTOR_GPU_BYTES: u64 = 256 * 1024 * 1024;
static VECTOR_PIPELINE_ID: AtomicU64 = AtomicU64::new(1);

pub const TEMPORAL_VECTOR_SHADER: &str = r#"
struct FrameConfig {
    instances_per_view: u32,
    active_views: u32,
    output_size: vec2<f32>,
}

struct VectorInstance {
    transform_x: vec4<f32>,
    transform_y: vec4<f32>,
    tint_alpha: vec4<f32>,
    visible: u32,
    blur: f32,
    has_blur_filter: u32,
    _padding: u32,
}

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
}

@group(0) @binding(0)
var<storage, read> instances: array<VectorInstance>;

@group(0) @binding(1)
var<uniform> frame: FrameConfig;

@vertex
fn vertex_main(
    @location(0) local: vec2<f32>,
    @location(1) style: vec4<f32>,
    @builtin(instance_index) slot: u32,
    @builtin(view_index) view_index: i32,
) -> VertexOutput {
    let view = u32(view_index);
    let instance = instances[view * frame.instances_per_view + slot];
    let pixel = vec2<f32>(
        dot(instance.transform_x.xy, local) + instance.transform_x.z,
        dot(instance.transform_y.xy, local) + instance.transform_y.z,
    );
    let clip = vec2<f32>(
        pixel.x / frame.output_size.x * 2.0 - 1.0,
        1.0 - pixel.y / frame.output_size.y * 2.0,
    );
    let alpha = style.a * instance.tint_alpha.a;
    var output: VertexOutput;
    output.position = vec4<f32>(clip, 0.0, 1.0);
    output.color = select(
        vec4<f32>(0.0),
        vec4<f32>(style.rgb * instance.tint_alpha.rgb * alpha, alpha),
        instance.visible != 0u && view < frame.active_views,
    );
    return output;
}

@fragment
fn fragment_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return input.color;
}
"#;

pub const VECTOR_FILTER_SHADER: &str = r#"
struct FrameConfig {
    instances_per_view: u32,
    active_views: u32,
    output_size: vec2<f32>,
}

struct VectorInstance {
    transform_x: vec4<f32>,
    transform_y: vec4<f32>,
    tint_alpha: vec4<f32>,
    visible: u32,
    blur: f32,
    has_blur_filter: u32,
    _padding: u32,
}

struct FilterConfig {
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
var<storage, read> instances: array<VectorInstance>;

@group(0) @binding(2)
var<uniform> frame: FrameConfig;

@group(0) @binding(3)
var<uniform> config: FilterConfig;

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
fn fragment_copy(input: VertexOutput) -> @location(0) vec4<f32> {
    let pixel = vec2<i32>(input.position.xy);
    return textureLoad(source, pixel, i32(input.layer), 0);
}

@fragment
fn fragment_clear_transparent() -> @location(0) vec4<f32> {
    return vec4<f32>(0.0);
}

@fragment
fn fragment_clear_multiply() -> @location(0) vec4<f32> {
    return vec4<f32>(1.0, 1.0, 1.0, 0.0);
}

@fragment
fn fragment_downsample(input: VertexOutput) -> @location(0) vec4<f32> {
    let base = vec2<i32>(input.position.xy) * 2;
    let layer = i32(input.layer);
    return (
        textureLoad(source, base, layer, 0)
        + textureLoad(source, base + vec2<i32>(1, 0), layer, 0)
        + textureLoad(source, base + vec2<i32>(0, 1), layer, 0)
        + textureLoad(source, base + vec2<i32>(1, 1), layer, 0)
    ) * 0.25;
}

@fragment
fn fragment_blur(input: VertexOutput) -> @location(0) vec4<f32> {
    let instance =
        instances[input.layer * frame.instances_per_view + config.instance_index];
    let center = input.position.xy - vec2<f32>(0.5);
    let layer = i32(input.layer);
    // Match Pixi's quality-four zero-strength behavior across heterogeneous
    // multiview layers. The final pass substitutes horizontal pass four.
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

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct VectorGpuInstance {
    pub transform_x: [f32; 4],
    pub transform_y: [f32; 4],
    pub tint_alpha: [f32; 4],
    pub visible: u32,
    pub blur: f32,
    pub has_blur_filter: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct VectorGpuVertex {
    pub position: [f32; 2],
    pub color_alpha: [f32; 4],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VectorDrawRun {
    pub activation_order: u32,
    pub layer_order: u32,
    pub blend_mode: SpriteBlendMode,
    pub geometry_id: VectorGeometryId,
    pub slot: u32,
    pub has_blur_filter: bool,
}

#[derive(Clone, Debug)]
pub struct TemporalVectorBatch {
    pub active_views: NonZeroU32,
    pub instances_per_view: u32,
    pub instances: Vec<VectorGpuInstance>,
    pub draw_runs: Vec<VectorDrawRun>,
    pub(crate) slot_activations: Vec<u32>,
    referenced_vertex_count: u32,
    validation_draw_runs: Vec<VectorDrawRun>,
}

impl TemporalVectorBatch {
    pub const fn referenced_vertex_count(&self) -> u32 {
        self.referenced_vertex_count
    }

    pub fn pack(views_per_batch: NonZeroU32, views: &[&[PreparedVector<'_>]]) -> Result<Self> {
        let capacity = views_per_batch.get();
        if !(SpritePipeline::MIN_VIEWS_PER_BATCH..=SpritePipeline::MAX_VIEWS_PER_BATCH)
            .contains(&capacity)
        {
            return Err(Error::Invalid(format!(
                "temporal vector batches require {} to {} configured views",
                SpritePipeline::MIN_VIEWS_PER_BATCH,
                SpritePipeline::MAX_VIEWS_PER_BATCH
            )));
        }
        let active_views =
            NonZeroU32::new(u32::try_from(views.len()).map_err(|_| Error::ArithmeticOverflow)?)
                .ok_or_else(|| {
                    Error::Invalid(
                        "temporal vector batch requires at least one active view".to_owned(),
                    )
                })?;
        if active_views.get() > capacity {
            return Err(Error::Invalid(
                "active temporal vector views exceed configured capacity".to_owned(),
            ));
        }

        #[derive(Clone, Copy)]
        struct Slot<'a> {
            layer_order: u32,
            blend_mode: SpriteBlendMode,
            mesh: &'a VectorMesh,
            has_blur_filter: bool,
        }

        let mut slots = BTreeMap::<u32, Slot<'_>>::new();
        let mut edges = BTreeMap::<u32, BTreeSet<u32>>::new();
        let mut indegrees = BTreeMap::<u32, u32>::new();
        let mut seen = BTreeSet::new();
        for view in views {
            seen.clear();
            for vector in *view {
                validate_prepared_vector(vector)?;
                if !seen.insert(vector.activation_order) {
                    return Err(Error::Invalid(
                        "prepared vector view repeats an activation".to_owned(),
                    ));
                }
                let slot = Slot {
                    layer_order: vector.layer_order,
                    blend_mode: vector.blend_mode,
                    mesh: vector.mesh,
                    has_blur_filter: vector.blur.is_some(),
                };
                if let Some(existing) = slots.insert(vector.activation_order, slot)
                    && (existing.layer_order != slot.layer_order
                        || existing.blend_mode != slot.blend_mode
                        || existing.mesh.geometry_id() != slot.mesh.geometry_id()
                        || existing.has_blur_filter != slot.has_blur_filter)
                {
                    return Err(Error::Invalid(format!(
                        "vector activation {} changes geometry, filter, or render identity across views",
                        vector.activation_order
                    )));
                }
                edges.entry(vector.activation_order).or_default();
                indegrees.entry(vector.activation_order).or_default();
            }
            for adjacent in view.windows(2) {
                let before = adjacent[0].activation_order;
                let after = adjacent[1].activation_order;
                if edges.entry(before).or_default().insert(after) {
                    let indegree = indegrees.entry(after).or_default();
                    *indegree = indegree.checked_add(1).ok_or(Error::ArithmeticOverflow)?;
                }
            }
        }
        let mut ready = indegrees
            .iter()
            .filter_map(|(activation, indegree)| (*indegree == 0).then_some(*activation))
            .collect::<BTreeSet<_>>();
        let mut slot_order = Vec::with_capacity(slots.len());
        while let Some(activation) = ready.pop_first() {
            slot_order.push(activation);
            for after in &edges[&activation] {
                let indegree = indegrees
                    .get_mut(after)
                    .expect("edge target has an indegree");
                *indegree -= 1;
                if *indegree == 0 {
                    ready.insert(*after);
                }
            }
        }
        if slot_order.len() != slots.len() {
            return Err(Error::Invalid(
                "vector display order changes incompatibly across temporal views".to_owned(),
            ));
        }

        let instances_per_view =
            u32::try_from(slot_order.len()).map_err(|_| Error::ArithmeticOverflow)?;
        let total_instances = (capacity as usize)
            .checked_mul(slot_order.len())
            .ok_or(Error::ArithmeticOverflow)?;
        if total_instances > MAX_TEMPORAL_VECTOR_INSTANCES {
            return Err(Error::Invalid(format!(
                "temporal vector batch exceeds the {MAX_TEMPORAL_VECTOR_INSTANCES}-instance limit"
            )));
        }
        let mut instances = Vec::with_capacity(total_instances);
        for view_index in 0..capacity as usize {
            let prepared = views
                .get(view_index)
                .copied()
                .unwrap_or_default()
                .iter()
                .map(|vector| (vector.activation_order, vector_gpu_instance(vector)))
                .collect::<BTreeMap<_, _>>();
            for activation in &slot_order {
                instances.push(
                    prepared
                        .get(activation)
                        .copied()
                        .unwrap_or_else(VectorGpuInstance::zeroed),
                );
            }
        }

        let total_vertices = slot_order.iter().try_fold(0usize, |total, activation| {
            total
                .checked_add(slots[activation].mesh.vertices().len())
                .ok_or(Error::ArithmeticOverflow)
        })?;
        if total_vertices > MAX_TEMPORAL_VECTOR_VERTICES {
            return Err(Error::Invalid(format!(
                "temporal vector batch exceeds the {MAX_TEMPORAL_VECTOR_VERTICES}-vertex limit"
            )));
        }
        let mut draw_runs = Vec::new();
        for (slot_index, activation) in slot_order.iter().enumerate() {
            let slot_index = u32::try_from(slot_index).map_err(|_| Error::ArithmeticOverflow)?;
            let slot = slots[activation];
            if !slot.mesh.vertices().is_empty() {
                draw_runs.push(VectorDrawRun {
                    activation_order: *activation,
                    layer_order: slot.layer_order,
                    blend_mode: slot.blend_mode,
                    geometry_id: slot.mesh.geometry_id(),
                    slot: slot_index,
                    has_blur_filter: slot.has_blur_filter,
                });
            }
        }

        Ok(Self {
            active_views,
            instances_per_view,
            instances,
            referenced_vertex_count: u32::try_from(total_vertices)
                .map_err(|_| Error::ArithmeticOverflow)?,
            validation_draw_runs: draw_runs.clone(),
            draw_runs,
            slot_activations: slot_order,
        })
    }
}

fn validate_prepared_vector(vector: &PreparedVector<'_>) -> Result<()> {
    let values = [
        vector.transform.a,
        vector.transform.b,
        vector.transform.c,
        vector.transform.d,
        vector.transform.tx,
        vector.transform.ty,
        vector.alpha,
        vector.blur.unwrap_or(0.0),
    ];
    if values
        .iter()
        .any(|value| !value.is_finite() || !(*value as f32).is_finite())
        || vector.mesh.vertices().iter().any(|vertex| {
            vertex
                .position
                .iter()
                .chain(vertex.color_alpha.iter())
                .any(|value| !value.is_finite())
        })
    {
        return Err(Error::Invalid(
            "prepared vector contains a non-finite GPU value".to_owned(),
        ));
    }
    Ok(())
}

fn vector_gpu_instance(vector: &PreparedVector<'_>) -> VectorGpuInstance {
    let tint = vector.tint;
    VectorGpuInstance {
        transform_x: [
            vector.transform.a as f32,
            vector.transform.c as f32,
            vector.transform.tx as f32,
            0.0,
        ],
        transform_y: [
            vector.transform.b as f32,
            vector.transform.d as f32,
            vector.transform.ty as f32,
            0.0,
        ],
        tint_alpha: [
            ((tint >> 16) & 0xff) as f32 / 255.0,
            ((tint >> 8) & 0xff) as f32 / 255.0,
            (tint & 0xff) as f32 / 255.0,
            vector.alpha as f32,
        ],
        visible: u32::from(vector.visible),
        blur: vector.blur.unwrap_or(0.0) as f32,
        has_blur_filter: u32::from(vector.blur.is_some()),
        padding: 0,
    }
}

pub struct VectorPipeline {
    raster_pipelines: VectorBlendPipelines,
    filter_pipeline: VectorFilterPipeline,
    _supersample_texture: wgpu::Texture,
    supersample_view: wgpu::TextureView,
    filter_ping_view: wgpu::TextureView,
    filter_pong_view: wgpu::TextureView,
    geometry_buffer: wgpu::Buffer,
    geometry_ranges: BTreeMap<VectorGeometryId, Range<u32>>,
    geometry_bounds: BTreeMap<VectorGeometryId, Option<[f32; 4]>>,
    slots: Vec<VectorBatchSlot>,
    output_size: [u32; 2],
    views_per_batch: NonZeroU32,
    max_instances_per_view: u32,
    identity: u64,
    renderer_identity: u64,
}

struct VectorBatchSlot {
    bind_group: wgpu::BindGroup,
    instance_buffer: wgpu::Buffer,
    frame_buffer: wgpu::Buffer,
    filter: VectorFilterSlot,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct VectorFilterConfig {
    instance_index: u32,
    pass_kind: u32,
    padding: [u32; 2],
}

struct VectorFilterPipeline {
    bind_group_layout: wgpu::BindGroupLayout,
    clear_transparent: wgpu::RenderPipeline,
    clear_multiply: wgpu::RenderPipeline,
    downsample: wgpu::RenderPipeline,
    blur: wgpu::RenderPipeline,
    blur_final: wgpu::RenderPipeline,
    composite: VectorBlendPipelines,
    config_stride: u32,
}

struct VectorBlendPipelines {
    normal: wgpu::RenderPipeline,
    add: wgpu::RenderPipeline,
    multiply: wgpu::RenderPipeline,
    screen: wgpu::RenderPipeline,
}

struct VectorFilterSlot {
    supersample_bind_group: wgpu::BindGroup,
    ping_bind_group: wgpu::BindGroup,
    pong_bind_group: wgpu::BindGroup,
    _config_buffer: wgpu::Buffer,
}

#[derive(Clone, Debug)]
struct ResidentVectorDrawRun {
    activation_order: u32,
    blend_mode: SpriteBlendMode,
    vertices: Range<u32>,
    slot: u32,
    has_blur_filter: bool,
    output_scissor: Option<ScissorRect>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScissorRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl ScissorRect {
    fn supersampled(self) -> Result<Self> {
        Ok(Self {
            x: self.x.checked_mul(2).ok_or(Error::ArithmeticOverflow)?,
            y: self.y.checked_mul(2).ok_or(Error::ArithmeticOverflow)?,
            width: self.width.checked_mul(2).ok_or(Error::ArithmeticOverflow)?,
            height: self
                .height
                .checked_mul(2)
                .ok_or(Error::ArithmeticOverflow)?,
        })
    }
}

pub(crate) struct EncodedTemporalVectorBatch {
    pipeline_identity: u64,
    slot_index: usize,
    draw_runs: Vec<ResidentVectorDrawRun>,
    activations: BTreeSet<u32>,
}

impl VectorPipeline {
    pub const fn output_size(&self) -> [u32; 2] {
        self.output_size
    }

    pub fn resident_geometry_count(&self) -> usize {
        self.geometry_ranges.len()
    }

    pub fn resident_vertex_count(&self) -> u32 {
        self.geometry_ranges
            .values()
            .map(|range| range.end - range.start)
            .sum()
    }

    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    pub fn create(
        device: &wgpu::Device,
        renderer: &TemporalSpriteRenderer,
        output_size: [u32; 2],
        views_per_batch: NonZeroU32,
        max_instances_per_view: NonZeroU32,
        in_flight_batches: NonZeroU32,
        meshes: &[&VectorMesh],
    ) -> Result<Self> {
        let identity = VECTOR_PIPELINE_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| Error::ArithmeticOverflow)?;
        if output_size.contains(&0) {
            return Err(Error::Invalid(
                "vector target dimensions must be nonzero".to_owned(),
            ));
        }
        let supersample_size = [
            output_size[0]
                .checked_mul(2)
                .ok_or(Error::ArithmeticOverflow)?,
            output_size[1]
                .checked_mul(2)
                .ok_or(Error::ArithmeticOverflow)?,
        ];
        if supersample_size
            .iter()
            .any(|extent| *extent > device.limits().max_texture_dimension_2d)
        {
            return Err(Error::Invalid(
                "vector supersample target exceeds the GPU texture limit".to_owned(),
            ));
        }
        if !(SpritePipeline::MIN_VIEWS_PER_BATCH..=SpritePipeline::MAX_VIEWS_PER_BATCH)
            .contains(&views_per_batch.get())
        {
            return Err(Error::Invalid(
                "vector multiview count is outside the portable range".to_owned(),
            ));
        }
        if !device.features().contains(wgpu::Features::MULTIVIEW) {
            return Err(Error::Invalid(
                "GPU device lacks required vector multiview support".to_owned(),
            ));
        }
        if in_flight_batches.get() > SpritePipeline::MAX_IN_FLIGHT_BATCHES {
            return Err(Error::Invalid(format!(
                "vector pipeline supports at most {} in-flight batches",
                SpritePipeline::MAX_IN_FLIGHT_BATCHES
            )));
        }
        if renderer.slot_count() != in_flight_batches.get() as usize {
            return Err(Error::Invalid(
                "vector and sprite renderers require the same in-flight ring size".to_owned(),
            ));
        }
        let (renderer_identity, filter_ping, filter_pong) = renderer.vector_filter_resources();
        for scratch in [filter_ping, filter_pong] {
            if [scratch.width, scratch.height] != output_size
                || scratch.layers != views_per_batch
                || scratch.format != PIXI_COLOR_FORMAT
            {
                return Err(Error::Invalid(
                    "vector pipeline differs from the shared temporal filter targets".to_owned(),
                ));
            }
        }
        validate_vector_color_budget(output_size, views_per_batch, in_flight_batches)?;
        let instance_count = views_per_batch
            .get()
            .checked_mul(max_instances_per_view.get())
            .ok_or(Error::ArithmeticOverflow)?;
        if usize::try_from(instance_count).map_err(|_| Error::ArithmeticOverflow)?
            > MAX_TEMPORAL_VECTOR_INSTANCES
        {
            return Err(Error::Invalid(format!(
                "vector ring exceeds the {MAX_TEMPORAL_VECTOR_INSTANCES}-instance limit"
            )));
        }
        let instance_bytes = u64::from(instance_count)
            .checked_mul(std::mem::size_of::<VectorGpuInstance>() as u64)
            .ok_or(Error::ArithmeticOverflow)?;
        let limits = device.limits();
        if instance_bytes > limits.max_buffer_size
            || instance_bytes > u64::from(limits.max_storage_buffer_binding_size)
        {
            return Err(Error::Invalid(
                "vector instance ring exceeds GPU storage-buffer limits".to_owned(),
            ));
        }
        let config_stride = filter_config_stride(device)?;
        let filter_config_bytes = u64::from(max_instances_per_view.get())
            .checked_mul(3)
            .and_then(|count| count.checked_mul(u64::from(config_stride)))
            .ok_or(Error::ArithmeticOverflow)?;
        let ring_bytes = instance_bytes
            .checked_add(std::mem::size_of::<FrameConfig>() as u64)
            .and_then(|bytes| bytes.checked_add(filter_config_bytes))
            .and_then(|bytes| bytes.checked_mul(u64::from(in_flight_batches.get())))
            .ok_or(Error::ArithmeticOverflow)?;
        let (geometry_buffer, geometry_ranges) = build_geometry_bank(device, meshes, ring_bytes)?;
        let geometry_bounds = build_geometry_bounds(meshes)?;
        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("temporal vector bindings"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Storage { read_only: true },
                            has_dynamic_offset: false,
                            min_binding_size: NonZeroU64::new(
                                std::mem::size_of::<VectorGpuInstance>() as u64,
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
                ],
            });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("temporal vector pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("temporal vector shader"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(TEMPORAL_VECTOR_SHADER)),
        });
        let raster_pipelines =
            VectorBlendPipelines::create_raster(device, &layout, &shader, views_per_batch);
        let filter_pipeline = VectorFilterPipeline::create(device, views_per_batch, config_stride)?;
        let supersample_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("temporal vector 2x supersample scratch"),
            size: wgpu::Extent3d {
                width: supersample_size[0],
                height: supersample_size[1],
                depth_or_array_layers: views_per_batch.get(),
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: PIXI_COLOR_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let supersample_view =
            supersample_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut slots = Vec::with_capacity(in_flight_batches.get() as usize);
        for _ in 0..in_flight_batches.get() {
            let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("temporal vector instance ring slot"),
                size: instance_bytes,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let frame_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("temporal vector frame ring slot"),
                size: std::mem::size_of::<FrameConfig>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("temporal vector ring bind group"),
                layout: &bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: instance_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: frame_buffer.as_entire_binding(),
                    },
                ],
            });
            let filter = VectorFilterSlot::create(
                device,
                &filter_pipeline,
                [&filter_ping.view, &filter_pong.view, &supersample_view],
                &instance_buffer,
                &frame_buffer,
                max_instances_per_view.get(),
            )?;
            slots.push(VectorBatchSlot {
                bind_group,
                instance_buffer,
                frame_buffer,
                filter,
            });
        }
        Ok(Self {
            raster_pipelines,
            filter_pipeline,
            _supersample_texture: supersample_texture,
            supersample_view,
            filter_ping_view: filter_ping.view.clone(),
            filter_pong_view: filter_pong.view.clone(),
            geometry_buffer,
            geometry_ranges,
            geometry_bounds,
            slots,
            output_size,
            views_per_batch,
            max_instances_per_view: max_instances_per_view.get(),
            identity,
            renderer_identity,
        })
    }

    pub(crate) fn prepare_batch(
        &self,
        queue: &wgpu::Queue,
        slot_index: usize,
        batch: &TemporalVectorBatch,
    ) -> Result<EncodedTemporalVectorBatch> {
        if batch.active_views.get() > self.views_per_batch.get() {
            return Err(Error::Invalid(
                "vector batch exceeds pipeline multiview capacity".to_owned(),
            ));
        }
        if batch.instances_per_view > self.max_instances_per_view {
            return Err(Error::Invalid(
                "temporal vector count exceeds the configured ring capacity".to_owned(),
            ));
        }
        validate_batch(batch, self.views_per_batch)?;
        let slot = self.slots.get(slot_index).ok_or_else(|| {
            Error::Invalid(format!("invalid temporal vector ring slot {slot_index}"))
        })?;
        let draw_runs = batch
            .draw_runs
            .iter()
            .map(|run| {
                let vertices = self
                    .geometry_ranges
                    .get(&run.geometry_id)
                    .cloned()
                    .ok_or_else(|| {
                        Error::Invalid(format!(
                            "vector activation {} is absent from the resident geometry bank",
                            run.activation_order
                        ))
                    })?;
                Ok(ResidentVectorDrawRun {
                    activation_order: run.activation_order,
                    blend_mode: run.blend_mode,
                    vertices,
                    slot: run.slot,
                    has_blur_filter: run.has_blur_filter,
                    output_scissor: self
                        .geometry_bounds
                        .get(&run.geometry_id)
                        .ok_or_else(|| {
                            Error::Invalid(format!(
                                "vector activation {} lacks resident geometry bounds",
                                run.activation_order
                            ))
                        })?
                        .as_ref()
                        .map(|bounds| output_scissor(bounds, batch, run.slot, self.output_size))
                        .transpose()?
                        .flatten(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if !batch.instances.is_empty() {
            queue.write_buffer(
                &slot.instance_buffer,
                0,
                bytemuck::cast_slice(&batch.instances),
            );
        }
        let frame = FrameConfig {
            instances_per_view: batch.instances_per_view,
            active_views: batch.active_views.get(),
            output_size: [self.output_size[0] as f32, self.output_size[1] as f32],
        };
        queue.write_buffer(&slot.frame_buffer, 0, bytemuck::bytes_of(&frame));
        Ok(EncodedTemporalVectorBatch {
            pipeline_identity: self.identity,
            slot_index,
            draw_runs,
            activations: batch.slot_activations.iter().copied().collect(),
        })
    }

    pub(crate) fn encode_prepared_activations(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        batch: &EncodedTemporalVectorBatch,
        activation_orders: &[u32],
        load: wgpu::LoadOp<wgpu::Color>,
    ) -> Result<()> {
        let mut runs = Vec::with_capacity(activation_orders.len());
        for activation_order in activation_orders {
            if !batch.activations.contains(activation_order) {
                return Err(Error::Invalid(format!(
                    "temporal vector batch lacks activation {activation_order}"
                )));
            }
            runs.extend(
                batch
                    .draw_runs
                    .iter()
                    .filter(|run| run.activation_order == *activation_order),
            );
        }
        self.encode_prepared_runs(encoder, target, batch, &runs, load)
    }

    fn encode_prepared_runs(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        batch: &EncodedTemporalVectorBatch,
        runs: &[&ResidentVectorDrawRun],
        load: wgpu::LoadOp<wgpu::Color>,
    ) -> Result<()> {
        if batch.pipeline_identity != self.identity {
            return Err(Error::Invalid(
                "encoded vector batch belongs to another pipeline".to_owned(),
            ));
        }
        if runs.is_empty() {
            if !matches!(load, wgpu::LoadOp::Load) {
                let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("empty temporal vector pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
            }
            return Ok(());
        }
        let slot = self.vector_slot(batch)?;
        let mut scene_load = load;
        for run in runs {
            let Some(output_scissor) = run.output_scissor else {
                continue;
            };
            self.encode_run_to_ping(encoder, slot, run)?;
            if run.has_blur_filter {
                self.encode_blurred_run(encoder, slot, target, run, scene_load)?;
            } else {
                let mut pass = begin_vector_target_pass(encoder, target, scene_load);
                self.draw_unfiltered_run(&mut pass, slot, run, output_scissor);
            }
            scene_load = wgpu::LoadOp::Load;
        }
        Ok(())
    }

    fn vector_slot<'a>(
        &'a self,
        batch: &EncodedTemporalVectorBatch,
    ) -> Result<&'a VectorBatchSlot> {
        self.slots.get(batch.slot_index).ok_or_else(|| {
            Error::Invalid(format!(
                "encoded vector batch references invalid ring slot {}",
                batch.slot_index
            ))
        })
    }

    fn encode_run_to_ping(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        slot: &VectorBatchSlot,
        run: &ResidentVectorDrawRun,
    ) -> Result<()> {
        let output_scissor = run
            .output_scissor
            .expect("material vector runs retain an output scissor");
        let scratch_clear = if !run.has_blur_filter && run.blend_mode == SpriteBlendMode::Multiply {
            wgpu::Color {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 0.0,
            }
        } else {
            wgpu::Color::TRANSPARENT
        };
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("temporal vector 2x supersample node"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.supersample_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: if run.has_blur_filter {
                            wgpu::LoadOp::Clear(scratch_clear)
                        } else {
                            wgpu::LoadOp::Load
                        },
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            if !run.has_blur_filter {
                let supersample_scissor = output_scissor.supersampled()?;
                pass.set_scissor_rect(
                    supersample_scissor.x,
                    supersample_scissor.y,
                    supersample_scissor.width,
                    supersample_scissor.height,
                );
                pass.set_pipeline(if run.blend_mode == SpriteBlendMode::Multiply {
                    &self.filter_pipeline.clear_multiply
                } else {
                    &self.filter_pipeline.clear_transparent
                });
                // The clear entry point does not sample binding 0. Bind a
                // different scratch view so wgpu does not conservatively mark
                // the supersample attachment as both input and output.
                pass.set_bind_group(0, &slot.filter.ping_bind_group, &[0]);
                pass.draw(0..3, 0..1);
            }
            pass.set_pipeline(self.raster_pipelines.for_blend(run.blend_mode));
            pass.set_bind_group(0, &slot.bind_group, &[]);
            pass.set_vertex_buffer(0, self.geometry_buffer.slice(..));
            pass.draw(run.vertices.clone(), run.slot..run.slot + 1);
        }
        encode_vector_filter_pass(
            encoder,
            &self.filter_pipeline.downsample,
            &slot.filter.supersample_bind_group,
            &self.filter_ping_view,
            0,
            if run.has_blur_filter {
                wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
            } else {
                // The full-screen resolve overwrites every pixel inside the
                // scissor, and only that region is composited.
                wgpu::LoadOp::Load
            },
            (!run.has_blur_filter).then_some(output_scissor),
        );
        Ok(())
    }

    fn draw_unfiltered_run(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        slot: &VectorBatchSlot,
        run: &ResidentVectorDrawRun,
        scissor: ScissorRect,
    ) {
        pass.set_scissor_rect(scissor.x, scissor.y, scissor.width, scissor.height);
        pass.set_pipeline(self.filter_pipeline.composite.for_blend(run.blend_mode));
        pass.set_bind_group(0, &slot.filter.ping_bind_group, &[0]);
        pass.draw(0..3, 0..1);
    }

    fn encode_blurred_run(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        slot: &VectorBatchSlot,
        target: &wgpu::TextureView,
        run: &ResidentVectorDrawRun,
        load: wgpu::LoadOp<wgpu::Color>,
    ) -> Result<()> {
        let horizontal_offset =
            filter_config_offset(run.slot, 1, self.filter_pipeline.config_stride)?;
        let vertical_offset =
            filter_config_offset(run.slot, 0, self.filter_pipeline.config_stride)?;
        let final_offset = filter_config_offset(run.slot, 2, self.filter_pipeline.config_stride)?;
        for (source, destination, offset, load) in [
            (
                &slot.filter.ping_bind_group,
                &self.filter_pong_view,
                horizontal_offset,
                wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
            ),
            (
                &slot.filter.pong_bind_group,
                &self.filter_ping_view,
                horizontal_offset,
                wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
            ),
            (
                &slot.filter.ping_bind_group,
                &self.filter_pong_view,
                horizontal_offset,
                wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
            ),
            (
                &slot.filter.pong_bind_group,
                &self.filter_ping_view,
                horizontal_offset,
                wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
            ),
            (
                &slot.filter.ping_bind_group,
                &self.filter_pong_view,
                vertical_offset,
                wgpu::LoadOp::Load,
            ),
            (
                &slot.filter.pong_bind_group,
                &self.filter_ping_view,
                vertical_offset,
                wgpu::LoadOp::Load,
            ),
            (
                &slot.filter.ping_bind_group,
                &self.filter_pong_view,
                vertical_offset,
                wgpu::LoadOp::Load,
            ),
        ] {
            encode_vector_filter_pass(
                encoder,
                &self.filter_pipeline.blur,
                source,
                destination,
                offset,
                load,
                None,
            );
        }
        encode_vector_filter_pass(
            encoder,
            &self.filter_pipeline.blur_final,
            &slot.filter.pong_bind_group,
            target,
            final_offset,
            load,
            None,
        );
        Ok(())
    }

    pub(crate) fn is_compatible_renderer(&self, renderer: &TemporalSpriteRenderer) -> bool {
        self.renderer_identity == renderer.identity()
    }
}

impl VectorBlendPipelines {
    fn create_raster(
        device: &wgpu::Device,
        layout: &wgpu::PipelineLayout,
        shader: &wgpu::ShaderModule,
        views_per_batch: NonZeroU32,
    ) -> Self {
        let create = |suffix, blend| {
            create_vector_raster_pipeline(
                device,
                layout,
                shader,
                views_per_batch,
                &format!("temporal vector supersample {suffix}"),
                blend,
            )
        };
        Self {
            normal: create("normal", wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
            add: create("add", gpu::additive_blend()),
            multiply: create("multiply", gpu::multiply_blend()),
            screen: create("screen", gpu::screen_blend()),
        }
    }

    fn for_blend(&self, blend: SpriteBlendMode) -> &wgpu::RenderPipeline {
        match blend {
            SpriteBlendMode::Normal => &self.normal,
            SpriteBlendMode::Add => &self.add,
            SpriteBlendMode::Multiply => &self.multiply,
            SpriteBlendMode::Screen => &self.screen,
        }
    }
}

fn validate_batch(batch: &TemporalVectorBatch, views_per_batch: NonZeroU32) -> Result<()> {
    let expected_instances = views_per_batch
        .get()
        .checked_mul(batch.instances_per_view)
        .ok_or(Error::ArithmeticOverflow)?;
    if usize::try_from(expected_instances).map_err(|_| Error::ArithmeticOverflow)?
        != batch.instances.len()
    {
        return Err(Error::Invalid(
            "vector instance layout is inconsistent".to_owned(),
        ));
    }
    if usize::try_from(batch.instances_per_view).map_err(|_| Error::ArithmeticOverflow)?
        != batch.slot_activations.len()
        || batch
            .slot_activations
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .len()
            != batch.slot_activations.len()
    {
        return Err(Error::Invalid(
            "vector activation slots are inconsistent".to_owned(),
        ));
    }
    if batch.draw_runs != batch.validation_draw_runs {
        return Err(Error::Invalid(
            "vector draw-run identity was modified after packing".to_owned(),
        ));
    }
    for run in &batch.draw_runs {
        let slot = usize::try_from(run.slot).map_err(|_| Error::ArithmeticOverflow)?;
        if batch.slot_activations.get(slot).copied() != Some(run.activation_order) {
            return Err(Error::Invalid(format!(
                "vector draw run {} uses another activation's instance slot",
                run.activation_order
            )));
        }
    }
    if batch.instances.iter().any(|instance| {
        instance
            .transform_x
            .iter()
            .chain(instance.transform_y.iter())
            .chain(instance.tint_alpha.iter())
            .any(|value| !value.is_finite())
            || !instance.blur.is_finite()
    }) {
        return Err(Error::Invalid(
            "vector batch contains a non-finite GPU value".to_owned(),
        ));
    }
    Ok(())
}

fn build_geometry_bank(
    device: &wgpu::Device,
    meshes: &[&VectorMesh],
    ring_bytes: u64,
) -> Result<(wgpu::Buffer, BTreeMap<VectorGeometryId, Range<u32>>)> {
    let mut unique = BTreeMap::<VectorGeometryId, &VectorMesh>::new();
    for mesh in meshes {
        if !mesh.vertices().len().is_multiple_of(3)
            || mesh.vertices().iter().any(|vertex| {
                vertex
                    .position
                    .iter()
                    .chain(vertex.color_alpha.iter())
                    .any(|value| !value.is_finite())
            })
        {
            return Err(Error::Invalid(
                "resident vector geometry is not a finite triangle list".to_owned(),
            ));
        }
        if let Some(existing) = unique.insert(mesh.geometry_id(), mesh)
            && existing.vertices() != mesh.vertices()
        {
            return Err(Error::Invalid(
                "distinct vector meshes collide on one geometry identity".to_owned(),
            ));
        }
    }
    let total_vertices = unique.values().try_fold(0usize, |total, mesh| {
        total
            .checked_add(mesh.vertices().len())
            .ok_or(Error::ArithmeticOverflow)
    })?;
    if total_vertices > MAX_TEMPORAL_VECTOR_VERTICES {
        return Err(Error::Invalid(format!(
            "resident vector bank exceeds the {MAX_TEMPORAL_VECTOR_VERTICES}-vertex limit"
        )));
    }
    let mut vertices = Vec::<VectorGpuVertex>::with_capacity(total_vertices);
    let mut ranges = BTreeMap::new();
    for (geometry_id, mesh) in unique {
        let start = u32::try_from(vertices.len()).map_err(|_| Error::ArithmeticOverflow)?;
        vertices.extend(mesh.vertices().iter().map(|vertex| VectorGpuVertex {
            position: vertex.position,
            color_alpha: vertex.color_alpha,
        }));
        let end = u32::try_from(vertices.len()).map_err(|_| Error::ArithmeticOverflow)?;
        ranges.insert(geometry_id, start..end);
    }
    let bytes = bytemuck::cast_slice(&vertices);
    let geometry_bytes = u64::try_from(bytes.len()).map_err(|_| Error::ArithmeticOverflow)?;
    if geometry_bytes > device.limits().max_buffer_size {
        return Err(Error::Invalid(
            "resident vector geometry exceeds the GPU buffer-size limit".to_owned(),
        ));
    }
    let total_gpu_bytes = geometry_bytes
        .checked_add(ring_bytes)
        .ok_or(Error::ArithmeticOverflow)?;
    if total_gpu_bytes > MAX_VECTOR_GPU_BYTES {
        return Err(Error::Invalid(format!(
            "resident vector geometry and ring require {total_gpu_bytes} bytes; limit is {MAX_VECTOR_GPU_BYTES}"
        )));
    }
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("resident vector geometry bank"),
        size: bytes.len().max(4) as u64,
        usage: wgpu::BufferUsages::VERTEX,
        mapped_at_creation: true,
    });
    if !bytes.is_empty() {
        buffer.slice(..bytes.len() as u64).get_mapped_range_mut()[..].copy_from_slice(bytes);
    }
    buffer.unmap();
    Ok((buffer, ranges))
}

fn build_geometry_bounds(
    meshes: &[&VectorMesh],
) -> Result<BTreeMap<VectorGeometryId, Option<[f32; 4]>>> {
    let mut bounds = BTreeMap::new();
    for mesh in meshes {
        let mesh_bounds = mesh.vertices().first().map(|first| {
            let mut mesh_bounds = [
                first.position[0],
                first.position[1],
                first.position[0],
                first.position[1],
            ];
            for vertex in &mesh.vertices()[1..] {
                mesh_bounds[0] = mesh_bounds[0].min(vertex.position[0]);
                mesh_bounds[1] = mesh_bounds[1].min(vertex.position[1]);
                mesh_bounds[2] = mesh_bounds[2].max(vertex.position[0]);
                mesh_bounds[3] = mesh_bounds[3].max(vertex.position[1]);
            }
            mesh_bounds
        });
        if let Some(existing) = bounds.insert(mesh.geometry_id(), mesh_bounds)
            && existing != mesh_bounds
        {
            return Err(Error::Invalid(
                "distinct vector meshes collide on one geometry identity".to_owned(),
            ));
        }
    }
    Ok(bounds)
}

fn output_scissor(
    geometry_bounds: &[f32; 4],
    batch: &TemporalVectorBatch,
    slot: u32,
    output_size: [u32; 2],
) -> Result<Option<ScissorRect>> {
    let slot = usize::try_from(slot).map_err(|_| Error::ArithmeticOverflow)?;
    let instances_per_view =
        usize::try_from(batch.instances_per_view).map_err(|_| Error::ArithmeticOverflow)?;
    let mut output_bounds: Option<[f32; 4]> = None;
    for view in 0..batch.active_views.get() as usize {
        let index = view
            .checked_mul(instances_per_view)
            .and_then(|index| index.checked_add(slot))
            .ok_or(Error::ArithmeticOverflow)?;
        let instance = batch.instances.get(index).ok_or_else(|| {
            Error::Invalid("vector scissor references an absent instance".to_owned())
        })?;
        if instance.visible == 0 {
            continue;
        }
        let [min_x, min_y, max_x, max_y] = *geometry_bounds;
        let mut view_bounds: Option<[f32; 4]> = None;
        for [x, y] in [
            [min_x, min_y],
            [max_x, min_y],
            [min_x, max_y],
            [max_x, max_y],
        ] {
            let transformed_x = instance.transform_x[0].mul_add(x, instance.transform_x[1] * y)
                + instance.transform_x[2];
            let transformed_y = instance.transform_y[0].mul_add(x, instance.transform_y[1] * y)
                + instance.transform_y[2];
            match &mut view_bounds {
                Some(bounds) => {
                    bounds[0] = bounds[0].min(transformed_x);
                    bounds[1] = bounds[1].min(transformed_y);
                    bounds[2] = bounds[2].max(transformed_x);
                    bounds[3] = bounds[3].max(transformed_y);
                }
                None => {
                    view_bounds =
                        Some([transformed_x, transformed_y, transformed_x, transformed_y]);
                }
            }
        }
        let mut view_bounds = view_bounds.expect("geometry bounds have four corners");
        if instance.has_blur_filter != 0 {
            // Four quality passes can move a sample by half the configured
            // strength each, for a total reach of twice the strength on each
            // axis. Include bilinear support so an offscreen filtered vector
            // is skipped only when its blur cannot affect the target.
            let reach = instance.blur.abs().mul_add(2.0, 2.0);
            view_bounds[0] -= reach;
            view_bounds[1] -= reach;
            view_bounds[2] += reach;
            view_bounds[3] += reach;
        }
        match &mut output_bounds {
            Some(bounds) => {
                bounds[0] = bounds[0].min(view_bounds[0]);
                bounds[1] = bounds[1].min(view_bounds[1]);
                bounds[2] = bounds[2].max(view_bounds[2]);
                bounds[3] = bounds[3].max(view_bounds[3]);
            }
            None => output_bounds = Some(view_bounds),
        }
    }
    let Some([min_x, min_y, max_x, max_y]) = output_bounds else {
        return Ok(None);
    };
    // The resolve samples a 2x raster at four subpixel centers. Expand one
    // output pixel so edge samples and floating-point rasterization rules can
    // never be clipped by the optimization.
    let min_x = (min_x.floor() as i64 - 1).clamp(0, i64::from(output_size[0]));
    let min_y = (min_y.floor() as i64 - 1).clamp(0, i64::from(output_size[1]));
    let max_x = (max_x.ceil() as i64 + 1).clamp(0, i64::from(output_size[0]));
    let max_y = (max_y.ceil() as i64 + 1).clamp(0, i64::from(output_size[1]));
    if max_x <= min_x || max_y <= min_y {
        return Ok(None);
    }
    Ok(Some(ScissorRect {
        x: u32::try_from(min_x).map_err(|_| Error::ArithmeticOverflow)?,
        y: u32::try_from(min_y).map_err(|_| Error::ArithmeticOverflow)?,
        width: u32::try_from(max_x - min_x).map_err(|_| Error::ArithmeticOverflow)?,
        height: u32::try_from(max_y - min_y).map_err(|_| Error::ArithmeticOverflow)?,
    }))
}

impl VectorFilterPipeline {
    fn create(
        device: &wgpu::Device,
        views_per_batch: NonZeroU32,
        config_stride: u32,
    ) -> Result<Self> {
        let config_size = std::mem::size_of::<VectorFilterConfig>() as u64;
        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("temporal vector filter bindings"),
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
                                std::mem::size_of::<VectorGpuInstance>() as u64,
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
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("temporal vector filter pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("temporal vector filter shader"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(VECTOR_FILTER_SHADER)),
        });
        let clear_transparent = create_filter_render_pipeline(
            device,
            &layout,
            &shader,
            views_per_batch,
            "temporal vector partial transparent clear",
            "fragment_clear_transparent",
            None,
        );
        let clear_multiply = create_filter_render_pipeline(
            device,
            &layout,
            &shader,
            views_per_batch,
            "temporal vector partial multiply clear",
            "fragment_clear_multiply",
            None,
        );
        let downsample = create_filter_render_pipeline(
            device,
            &layout,
            &shader,
            views_per_batch,
            "temporal vector four-sample resolve",
            "fragment_downsample",
            None,
        );
        let blur = create_filter_render_pipeline(
            device,
            &layout,
            &shader,
            views_per_batch,
            "temporal vector blur intermediate",
            "fragment_blur",
            None,
        );
        let blur_final = create_filter_render_pipeline(
            device,
            &layout,
            &shader,
            views_per_batch,
            "temporal vector blur final normal",
            "fragment_blur",
            Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
        );
        let composite = VectorBlendPipelines::create_composite(
            device,
            &layout,
            &shader,
            views_per_batch,
            "supersample composite",
            "fragment_copy",
        );
        Ok(Self {
            bind_group_layout,
            clear_transparent,
            clear_multiply,
            downsample,
            blur,
            blur_final,
            composite,
            config_stride,
        })
    }
}

impl VectorBlendPipelines {
    fn create_composite(
        device: &wgpu::Device,
        layout: &wgpu::PipelineLayout,
        shader: &wgpu::ShaderModule,
        views_per_batch: NonZeroU32,
        label_prefix: &str,
        fragment_entry: &'static str,
    ) -> Self {
        let create = |suffix, blend| {
            create_filter_render_pipeline(
                device,
                layout,
                shader,
                views_per_batch,
                &format!("temporal vector {label_prefix} {suffix}"),
                fragment_entry,
                Some(blend),
            )
        };
        Self {
            normal: create("normal", wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
            add: create("add", gpu::additive_blend()),
            multiply: create("multiply", multiply_factor_composite()),
            screen: create("screen", gpu::screen_blend()),
        }
    }
}

fn multiply_factor_composite() -> wgpu::BlendState {
    wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::Dst,
            dst_factor: wgpu::BlendFactor::Zero,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent::OVER,
    }
}

impl VectorFilterSlot {
    fn create(
        device: &wgpu::Device,
        pipeline: &VectorFilterPipeline,
        sources: [&wgpu::TextureView; 3],
        instance_buffer: &wgpu::Buffer,
        frame_buffer: &wgpu::Buffer,
        max_instances_per_view: u32,
    ) -> Result<Self> {
        let config_bytes = u64::from(max_instances_per_view)
            .checked_mul(3)
            .and_then(|count| count.checked_mul(u64::from(pipeline.config_stride)))
            .ok_or(Error::ArithmeticOverflow)?;
        if config_bytes > device.limits().max_buffer_size || config_bytes > u64::from(u32::MAX) {
            return Err(Error::Invalid(
                "temporal vector filter configuration exceeds GPU buffer limits".to_owned(),
            ));
        }
        let config_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("temporal vector filter configurations"),
            size: config_bytes,
            usage: wgpu::BufferUsages::UNIFORM,
            mapped_at_creation: true,
        });
        {
            let mut mapped = config_buffer.slice(..).get_mapped_range_mut();
            for instance_index in 0..max_instances_per_view {
                for pass_kind in 0..3 {
                    let config = VectorFilterConfig {
                        instance_index,
                        pass_kind,
                        padding: [0; 2],
                    };
                    let offset = usize::try_from(
                        u64::from(instance_index)
                            .checked_mul(3)
                            .and_then(|index| index.checked_add(u64::from(pass_kind)))
                            .and_then(|index| index.checked_mul(u64::from(pipeline.config_stride)))
                            .ok_or(Error::ArithmeticOverflow)?,
                    )
                    .map_err(|_| Error::ArithmeticOverflow)?;
                    let bytes = bytemuck::bytes_of(&config);
                    mapped[offset..offset + bytes.len()].copy_from_slice(bytes);
                }
            }
        }
        config_buffer.unmap();
        let bind_group = |label, source: &wgpu::TextureView| {
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
                            size: NonZeroU64::new(std::mem::size_of::<VectorFilterConfig>() as u64),
                        }),
                    },
                ],
            })
        };
        let [ping, pong, supersample] = sources;
        let supersample_bind_group =
            bind_group("temporal vector filter from supersample", supersample);
        let ping_bind_group = bind_group("temporal vector filter from ping", ping);
        let pong_bind_group = bind_group("temporal vector filter from pong", pong);
        Ok(Self {
            supersample_bind_group,
            ping_bind_group,
            pong_bind_group,
            _config_buffer: config_buffer,
        })
    }
}

fn create_vector_raster_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    views_per_batch: NonZeroU32,
    label: &str,
    blend: wgpu::BlendState,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vertex_main"),
            compilation_options: Default::default(),
            buffers: &[wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<VectorGpuVertex>() as u64,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &[
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x2,
                        offset: 0,
                        shader_location: 0,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x4,
                        offset: 8,
                        shader_location: 1,
                    },
                ],
            }],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fragment_main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: PIXI_COLOR_FORMAT,
                blend: Some(blend),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview: Some(views_per_batch),
        cache: None,
    })
}

fn create_filter_render_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    views_per_batch: NonZeroU32,
    label: &str,
    fragment_entry: &'static str,
    blend: Option<wgpu::BlendState>,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
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
            entry_point: Some(fragment_entry),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: PIXI_COLOR_FORMAT,
                blend,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview: Some(views_per_batch),
        cache: None,
    })
}

fn filter_config_stride(device: &wgpu::Device) -> Result<u32> {
    let config_size = std::mem::size_of::<VectorFilterConfig>() as u64;
    let alignment = u64::from(device.limits().min_uniform_buffer_offset_alignment.max(1));
    u32::try_from(
        config_size
            .checked_next_multiple_of(alignment)
            .ok_or(Error::ArithmeticOverflow)?,
    )
    .map_err(|_| Error::ArithmeticOverflow)
}

fn filter_config_offset(instance: u32, pass_kind: u32, stride: u32) -> Result<u32> {
    if pass_kind >= 3 {
        return Err(Error::Invalid(
            "vector filter pass kind is out of range".to_owned(),
        ));
    }
    instance
        .checked_mul(3)
        .and_then(|index| index.checked_add(pass_kind))
        .and_then(|index| index.checked_mul(stride))
        .ok_or(Error::ArithmeticOverflow)
}

fn encode_vector_filter_pass(
    encoder: &mut wgpu::CommandEncoder,
    pipeline: &wgpu::RenderPipeline,
    bind_group: &wgpu::BindGroup,
    target: &wgpu::TextureView,
    dynamic_offset: u32,
    load: wgpu::LoadOp<wgpu::Color>,
    scissor: Option<ScissorRect>,
) {
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("temporal vector filter pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: target,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load,
                store: wgpu::StoreOp::Store,
            },
        })],
        ..Default::default()
    });
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, bind_group, &[dynamic_offset]);
    if let Some(scissor) = scissor {
        pass.set_scissor_rect(scissor.x, scissor.y, scissor.width, scissor.height);
    }
    pass.draw(0..3, 0..1);
}

fn begin_vector_target_pass<'a>(
    encoder: &'a mut wgpu::CommandEncoder,
    target: &'a wgpu::TextureView,
    load: wgpu::LoadOp<wgpu::Color>,
) -> wgpu::RenderPass<'a> {
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("temporal vector scene composite"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: target,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load,
                store: wgpu::StoreOp::Store,
            },
        })],
        ..Default::default()
    })
}

fn validate_vector_color_budget(
    output_size: [u32; 2],
    views_per_batch: NonZeroU32,
    in_flight_batches: NonZeroU32,
) -> Result<()> {
    // Sprite ring targets + two shared RGBA filter targets + one 2x-by-2x
    // vector target. Count the supersample target as four texture equivalents.
    let texture_equivalents = u64::from(in_flight_batches.get())
        .checked_add(6)
        .ok_or(Error::ArithmeticOverflow)?;
    let bytes = u64::from(output_size[0])
        .checked_mul(u64::from(output_size[1]))
        .and_then(|value| value.checked_mul(u64::from(views_per_batch.get())))
        .and_then(|value| value.checked_mul(4))
        .and_then(|value| value.checked_mul(texture_equivalents))
        .ok_or(Error::ArithmeticOverflow)?;
    if bytes > SpritePipeline::MAX_TEMPORAL_COLOR_BYTES {
        return Err(Error::Invalid(format!(
            "supersampled temporal color targets require {bytes} bytes; limit is {}",
            SpritePipeline::MAX_TEMPORAL_COLOR_BYTES
        )));
    }
    Ok(())
}

pub fn validate_vector_shader() -> Result<()> {
    for (label, source) in [
        ("temporal vector", TEMPORAL_VECTOR_SHADER),
        ("vector filter", VECTOR_FILTER_SHADER),
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
    use std::num::NonZeroU32;

    use crate::{
        Affine2, PreparedVector, SpriteBlendMode, TemporalVectorBatch, VectorCommand,
        VectorFillStyle, VectorProgram, tessellate_vector_program, validate_vector_shader,
    };

    #[test]
    fn vector_shader_validates_and_host_layouts_match() {
        validate_vector_shader().unwrap();
        assert_eq!(std::mem::size_of::<super::VectorGpuInstance>(), 64);
        assert_eq!(std::mem::size_of::<super::VectorGpuVertex>(), 24);
    }

    #[test]
    fn temporal_batch_pads_views_and_keeps_activation_runs_separate() {
        let mesh = tessellate_vector_program(&VectorProgram {
            commands: vec![
                VectorCommand::BeginFill(VectorFillStyle {
                    color: 0xff_ff_ff,
                    alpha: 1.0,
                }),
                VectorCommand::Rect {
                    origin: [0.0, 0.0],
                    size: [10.0, 10.0],
                },
            ],
        })
        .unwrap();
        let first = PreparedVector {
            entity_id: "one",
            node_id: "first",
            layer: None,
            layer_order: 0,
            z_index: 0.0,
            activation_order: 1,
            transform: Affine2 {
                a: 1.0,
                b: 0.0,
                c: 0.0,
                d: 1.0,
                tx: 0.0,
                ty: 0.0,
            },
            mesh: &mesh,
            alpha: 1.0,
            tint: 0xff_ff_ff,
            visible: true,
            blend_mode: SpriteBlendMode::Normal,
            blur: None,
        };
        let second = PreparedVector {
            activation_order: 2,
            node_id: "second",
            blend_mode: SpriteBlendMode::Add,
            ..first.clone()
        };
        let view = vec![first, second];
        let batch =
            TemporalVectorBatch::pack(NonZeroU32::new(2).unwrap(), &[view.as_slice()]).unwrap();
        assert_eq!(batch.active_views.get(), 1);
        assert_eq!(batch.instances_per_view, 2);
        assert_eq!(batch.instances.len(), 4);
        assert_eq!(batch.draw_runs.len(), 2);
        assert_eq!(batch.draw_runs[0].activation_order, 1);
        assert_eq!(batch.draw_runs[1].activation_order, 2);
        assert!(
            batch.instances[2..]
                .iter()
                .all(|instance| instance.visible == 0)
        );
        super::validate_batch(&batch, NonZeroU32::new(2).unwrap()).unwrap();

        let mut invalid = batch.clone();
        invalid.draw_runs[0].slot = invalid.instances_per_view;
        assert!(super::validate_batch(&invalid, NonZeroU32::new(2).unwrap()).is_err());

        let mut invalid = batch.clone();
        invalid.draw_runs[0].slot = 1;
        assert!(super::validate_batch(&invalid, NonZeroU32::new(2).unwrap()).is_err());

        let mut invalid = batch.clone();
        invalid.draw_runs[0].activation_order = 2;
        assert!(super::validate_batch(&invalid, NonZeroU32::new(2).unwrap()).is_err());

        let mut invalid = batch.clone();
        invalid.instances[0].transform_x[0] = f32::NAN;
        assert!(super::validate_batch(&invalid, NonZeroU32::new(2).unwrap()).is_err());

        let mut nonfinite = view[0].clone();
        nonfinite.transform.tx = f64::INFINITY;
        assert!(
            TemporalVectorBatch::pack(
                NonZeroU32::new(2).unwrap(),
                &[std::slice::from_ref(&nonfinite)]
            )
            .is_err()
        );

        let mut filtered = view[0].clone();
        filtered.blur = Some(3.5);
        let filtered_batch = TemporalVectorBatch::pack(
            NonZeroU32::new(2).unwrap(),
            &[std::slice::from_ref(&filtered)],
        )
        .unwrap();
        assert!(filtered_batch.draw_runs[0].has_blur_filter);
        assert_eq!(filtered_batch.instances[0].blur, 3.5);
        assert_eq!(filtered_batch.instances[0].has_blur_filter, 1);

        let unfiltered = view[0].clone();
        assert!(
            TemporalVectorBatch::pack(
                NonZeroU32::new(2).unwrap(),
                &[
                    std::slice::from_ref(&filtered),
                    std::slice::from_ref(&unfiltered)
                ],
            )
            .is_err()
        );
    }

    #[test]
    fn bounds_filter_offsets_and_supersampled_color_targets() {
        let stride = 256;
        assert_eq!(super::filter_config_offset(0, 0, stride).unwrap(), 0);
        assert_eq!(super::filter_config_offset(0, 1, stride).unwrap(), 256);
        assert_eq!(super::filter_config_offset(0, 2, stride).unwrap(), 512);
        assert_eq!(super::filter_config_offset(1, 0, stride).unwrap(), 768);
        assert!(super::filter_config_offset(0, 3, stride).is_err());

        let views = NonZeroU32::new(6).unwrap();
        let slots = NonZeroU32::new(3).unwrap();
        super::validate_vector_color_budget([1920, 1080], views, slots).unwrap();
        assert!(super::validate_vector_color_budget([3840, 2160], views, slots).is_err());
    }

    #[test]
    fn output_scissor_bounds_transformed_visible_geometry_conservatively() {
        let mesh = tessellate_vector_program(&VectorProgram {
            commands: vec![
                VectorCommand::BeginFill(VectorFillStyle {
                    color: 0xff_ff_ff,
                    alpha: 1.0,
                }),
                VectorCommand::Rect {
                    origin: [0.0, 0.0],
                    size: [10.0, 10.0],
                },
            ],
        })
        .unwrap();
        let vector = PreparedVector {
            entity_id: "bounded",
            node_id: "bounded",
            layer: None,
            layer_order: 0,
            z_index: 0.0,
            activation_order: 1,
            transform: Affine2 {
                a: 2.0,
                b: 0.0,
                c: 0.0,
                d: 2.0,
                tx: 10.25,
                ty: 20.75,
            },
            mesh: &mesh,
            alpha: 1.0,
            tint: 0xff_ff_ff,
            visible: true,
            blend_mode: SpriteBlendMode::Normal,
            blur: None,
        };
        let batch = TemporalVectorBatch::pack(
            NonZeroU32::new(2).unwrap(),
            &[std::slice::from_ref(&vector)],
        )
        .unwrap();
        assert_eq!(
            super::output_scissor(&[0.0, 0.0, 10.0, 10.0], &batch, 0, [100, 100]).unwrap(),
            Some(super::ScissorRect {
                x: 9,
                y: 19,
                width: 23,
                height: 23,
            })
        );

        let blurred_offscreen = PreparedVector {
            transform: Affine2 {
                a: 1.0,
                b: 0.0,
                c: 0.0,
                d: 1.0,
                tx: -15.0,
                ty: 10.0,
            },
            blur: Some(4.0),
            ..vector.clone()
        };
        let batch = TemporalVectorBatch::pack(
            NonZeroU32::new(2).unwrap(),
            &[std::slice::from_ref(&blurred_offscreen)],
        )
        .unwrap();
        assert!(
            super::output_scissor(&[0.0, 0.0, 10.0, 10.0], &batch, 0, [100, 100])
                .unwrap()
                .is_some()
        );

        let invisible = PreparedVector {
            visible: false,
            ..vector
        };
        let batch = TemporalVectorBatch::pack(
            NonZeroU32::new(2).unwrap(),
            &[std::slice::from_ref(&invisible)],
        )
        .unwrap();
        assert_eq!(
            super::output_scissor(&[0.0, 0.0, 10.0, 10.0], &batch, 0, [100, 100]).unwrap(),
            None
        );
    }

    #[test]
    fn empty_geometry_has_an_explicit_non_drawing_bound() {
        let mesh = tessellate_vector_program(&VectorProgram::default()).unwrap();
        let bounds = super::build_geometry_bounds(&[&mesh]).unwrap();
        assert_eq!(bounds.get(&mesh.geometry_id()), Some(&None));
    }

    #[test]
    fn multiply_resolve_composites_the_accumulated_factor_once() {
        let blend = super::multiply_factor_composite();
        assert_eq!(blend.color.src_factor, wgpu::BlendFactor::Dst);
        assert_eq!(blend.color.dst_factor, wgpu::BlendFactor::Zero);
        assert_eq!(blend.alpha, wgpu::BlendComponent::OVER);

        // Two half-alpha blue-zero yellow primitives multiply the blue
        // destination factor from 1 -> 1/2 -> 1/4. Applying that resolved
        // factor once to a quarter-gray scene matches sequential Pixi draws.
        let source_blue_factor = 0.5_f32;
        let resolved_factor = source_blue_factor * source_blue_factor;
        assert_eq!(0.25_f32 * resolved_factor, 0.0625);
    }
}
