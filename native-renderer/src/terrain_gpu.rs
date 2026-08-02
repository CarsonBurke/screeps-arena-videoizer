use std::collections::BTreeMap;
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::Arc;
use std::{borrow::Cow, ops::Range};

use bytemuck::{Pod, Zeroable};
use sha2::{Digest, Sha256};

use crate::{
    BoardTransform, Error, FrameConfig, FrameSample, GpuTerrainBlurBank, GpuTerrainWallBank,
    GpuTextureAtlas, PIXI_COLOR_FORMAT, Result, SpriteBlendMode, SpritePipeline, TerrainCoverage,
    TerrainDrawOp, TerrainDrawPhase, TerrainDrawPlan, TerrainDrawSource, TerrainGeometryTimeline,
    TerrainLayerComposite, TerrainPaintStyle, TerrainRasterMask, TerrainRasterMasks,
    TerrainTextureSample, TextureAtlas, Timeline,
    gpu::{additive_blend, multiply_blend, screen_blend},
    mip::downsample_r8,
};

pub const TERRAIN_MASK_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;
/// Conservative per-bank ceiling. Callers should partition replay geometry
/// into temporal windows when one resident terrain bank would exceed it.
pub const DEFAULT_TERRAIN_BANK_BYTE_BUDGET: u64 = 256 * 1024 * 1024;
const NO_TEXTURE_OR_MASK: u32 = u32::MAX;

pub const TERRAIN_DRAW_SHADER: &str = r#"
struct FrameConfig {
    instances_per_view: u32,
    active_views: u32,
    output_size: vec2<f32>,
}

struct TerrainInstance {
    transform_x: vec4<f32>,
    transform_y: vec4<f32>,
    atlas_uv: vec4<f32>,
    texture_info: vec4<f32>,
    tile_position_size: vec4<f32>,
    alpha_mask: vec4<f32>,
    fill_color: vec4<f32>,
    stroke_color: vec4<f32>,
    tint: vec4<f32>,
    source_layers: vec4<u32>,
    mask_info: vec4<u32>,
    additive_uv: vec4<f32>,
    additive_info: vec4<f32>,
    additive_position_alpha: vec4<f32>,
    additive_tint: vec4<f32>,
}

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) corner: vec2<f32>,
    @location(1) @interpolate(flat) instance_index: u32,
    @location(2) @interpolate(flat) view_index: u32,
}

@group(0) @binding(0)
var<storage, read> instances: array<TerrainInstance>;

@group(0) @binding(1)
var<uniform> frame: FrameConfig;

@group(0) @binding(2)
var atlas: texture_2d_array<f32>;

@group(0) @binding(3)
var atlas_sampler: sampler;

@group(0) @binding(4)
var masks: texture_2d_array<f32>;

@group(0) @binding(5)
var mask_sampler: sampler;

@group(0) @binding(6)
var wall_textures: texture_2d_array<f32>;

@group(0) @binding(7)
var wall_sampler: sampler;

@group(0) @binding(8)
var blurred_textures: texture_2d_array<f32>;

@group(0) @binding(9)
var blur_sampler: sampler;

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
    let rect_size = max(rect_max - rect_min, vec2<i32>(1));
    let divisor = i32(1u << level);
    let level_min = rect_min / divisor;
    let level_size = max(rect_size / divisor, vec2<i32>(1));
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

fn rect_lod(uv_rect: vec4<f32>, coordinate: vec2<f32>) -> f32 {
    let atlas_size = vec2<f32>(textureDimensions(atlas).xy);
    let rect_size = max(
        (uv_rect.zw - uv_rect.xy) * atlas_size,
        vec2<f32>(1.0),
    );
    let texel_derivative_x = dpdx(coordinate) * rect_size;
    let texel_derivative_y = dpdy(coordinate) * rect_size;
    let footprint = max(
        dot(texel_derivative_x, texel_derivative_x),
        dot(texel_derivative_y, texel_derivative_y),
    );
    return clamp(
        0.5 * log2(max(footprint, 1.0)),
        0.0,
        f32(textureNumLevels(atlas) - 1u),
    );
}

fn clamped_rect_sample(
    uv_rect: vec4<f32>,
    local: vec2<f32>,
    page: u32,
    lod: f32,
) -> vec4<f32> {
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

fn tiled_atlas_sample(
    uv_rect: vec4<f32>,
    texture_info: vec4<f32>,
    tile_position: vec2<f32>,
    local_position: vec2<f32>,
    page: u32,
    simple_repeat: bool,
    mipmap: bool,
) -> vec4<f32> {
    let normalized = (local_position - tile_position)
        / (texture_info.zw * texture_info.xy);
    let local = fract(normalized);
    if simple_repeat {
        let mapped = mix(uv_rect.xy, uv_rect.zw, local);
        if mipmap {
            return textureSampleGrad(
                atlas,
                atlas_sampler,
                mapped,
                i32(page),
                dpdx(normalized) * (uv_rect.zw - uv_rect.xy),
                dpdy(normalized) * (uv_rect.zw - uv_rect.xy),
            );
        }
        return textureSampleLevel(
            atlas,
            atlas_sampler,
            mapped,
            i32(page),
            0.0,
        );
    }
    let atlas_size = vec2<f32>(textureDimensions(atlas).xy);
    let rect_size = max(
        (uv_rect.zw - uv_rect.xy) * atlas_size,
        vec2<f32>(1.0),
    );
    let texel = local * rect_size - vec2<f32>(0.5);
    let hits_clamp = any(texel < vec2<f32>(0.0))
        || any(texel > rect_size - vec2<f32>(1.0));
    let lod = select(0.0, rect_lod(uv_rect, local), mipmap && !hits_clamp);
    return clamped_rect_sample(uv_rect, local, page, lod);
}

fn atlas_sample(
    instance: TerrainInstance,
    corner: vec2<f32>,
) -> vec4<f32> {
    if all(instance.texture_info.zw == vec2<f32>(0.0)) {
        let lod = select(
            0.0,
            rect_lod(instance.atlas_uv, corner),
            instance.tint.a != 0.0,
        );
        return clamped_rect_sample(
            instance.atlas_uv,
            corner,
            instance.source_layers.y,
            lod,
        );
    }
    return tiled_atlas_sample(
        instance.atlas_uv,
        instance.texture_info,
        instance.tile_position_size.xy,
        corner * instance.tile_position_size.zw,
        instance.source_layers.y,
        instance.alpha_mask.z != 0.0,
        instance.tint.a != 0.0,
    );
}

fn mask_sample(layer: u32, corner: vec2<f32>) -> f32 {
    return textureSample(masks, mask_sampler, corner, i32(layer)).r;
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
    let pixel_position = vec2<f32>(
        dot(instance.transform_x.xy, corner) + instance.transform_x.z,
        dot(instance.transform_y.xy, corner) + instance.transform_y.z,
    );
    let clip = vec2<f32>(
        pixel_position.x / frame.output_size.x * 2.0 - 1.0,
        1.0 - pixel_position.y / frame.output_size.y * 2.0,
    );
    var output: VertexOutput;
    output.position = vec4<f32>(clip, 0.0, 1.0);
    output.corner = corner;
    output.instance_index = instance_index;
    output.view_index = view;
    return output;
}

@fragment
fn fragment_main(input: VertexOutput) -> @location(0) vec4<f32> {
    if input.view_index >= frame.active_views {
        return vec4<f32>(0.0);
    }
    let instance = instances[
        input.view_index * frame.instances_per_view + input.instance_index
    ];
    var mask_coverage = 1.0;
    if instance.mask_info.y != 0u {
        mask_coverage = mask_sample(instance.mask_info.x, input.corner)
            * instance.alpha_mask.y;
        // A masked tiler covers the whole board geometrically. Reject empty
        // mask texels before its substantially more expensive atlas sample.
        if mask_coverage == 0.0 {
            discard;
        }
    }
    let kind = instance.source_layers.x;
    var color = vec4<f32>(0.0);
    if kind == 0u {
        color = instance.fill_color;
    } else if kind == 1u {
        color = atlas_sample(instance, input.corner);
        color = vec4<f32>(color.rgb * instance.tint.rgb, color.a);
    } else if kind == 2u {
        let fill_alpha = mask_sample(instance.source_layers.z, input.corner);
        let stroke_alpha = mask_sample(instance.source_layers.w, input.corner);
        let fill = vec4<f32>(instance.fill_color.rgb * fill_alpha, fill_alpha);
        let stroke = vec4<f32>(instance.stroke_color.rgb * stroke_alpha, stroke_alpha);
        color = fill + stroke;
        color = vec4<f32>(color.rgb * instance.tint.rgb, color.a);
        if instance.mask_info.z != 0xffffffffu {
            let sample = tiled_atlas_sample(
                instance.additive_uv,
                instance.additive_info,
                instance.additive_position_alpha.xy,
                input.corner * instance.tile_position_size.zw,
                instance.mask_info.z,
                instance.alpha_mask.w != 0.0,
                instance.additive_tint.a != 0.0,
            );
            let mask_alpha = mask_sample(instance.mask_info.w, input.corner);
            let alpha = instance.additive_position_alpha.z * mask_alpha;
            let additive = vec4<f32>(
                sample.rgb * instance.additive_tint.rgb * alpha,
                sample.a * alpha,
            );
            color = min(color + additive, vec4<f32>(1.0));
        }
    } else if kind == 3u {
        let coverage = mask_sample(instance.source_layers.z, input.corner);
        color = vec4<f32>(instance.fill_color.rgb * coverage, coverage);
    } else if kind == 4u {
        color = textureSample(
            wall_textures,
            wall_sampler,
            input.corner,
            i32(instance.source_layers.z),
        );
    } else if kind == 5u {
        color = textureSample(
            blurred_textures,
            blur_sampler,
            input.corner,
            i32(instance.source_layers.z),
        );
    }
    color *= mask_coverage;
    let alpha = instance.alpha_mask.x;
    return vec4<f32>(color.rgb * alpha, color.a * alpha);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct TerrainGpuInstance {
    pub transform_x: [f32; 4],
    pub transform_y: [f32; 4],
    pub atlas_uv: [f32; 4],
    pub texture_info: [f32; 4],
    /// `[tile x, tile y, local width, local height]`.
    pub tile_position_size: [f32; 4],
    /// `[operation alpha, mask alpha, primary simple tiler, additive simple tiler]`.
    pub alpha_mask: [f32; 4],
    pub fill_color: [f32; 4],
    pub stroke_color: [f32; 4],
    pub tint: [f32; 4],
    /// `[source kind, atlas page, fill/coverage layer, stroke layer]`.
    pub source_layers: [u32; 4],
    /// `[mask layer, mask enabled, additive atlas page, additive mask layer]`.
    pub mask_info: [u32; 4],
    pub additive_uv: [f32; 4],
    pub additive_info: [f32; 4],
    /// `[tile x, tile y, alpha, reserved]`.
    pub additive_position_alpha: [f32; 4],
    pub additive_tint: [f32; 4],
}

pub struct TerrainPipeline {
    pub bind_group_layout: wgpu::BindGroupLayout,
    pub normal: wgpu::RenderPipeline,
    pub add: wgpu::RenderPipeline,
    pub multiply: wgpu::RenderPipeline,
    pub screen: wgpu::RenderPipeline,
    views_per_batch: NonZeroU32,
}

pub struct TerrainGpuBindings {
    pub bind_group: wgpu::BindGroup,
    pub instance_buffer: wgpu::Buffer,
    pub frame_buffer: wgpu::Buffer,
    pub capacity: u32,
    atlas_layers: u32,
    mask_layers: u32,
    wall_layers: u32,
    blur_layers: u32,
}

/// One-shot upload storage for all terrain passes recorded into a command
/// buffer. Each pass writes a disjoint source range, then records an ordered
/// copy into the draw bindings immediately before its render pass.
pub struct TerrainCommandUploads {
    instance_buffer: wgpu::Buffer,
    frame_buffer: wgpu::Buffer,
    instance_stride: u64,
    instance_capacity: u32,
    pass_capacity: u32,
    next_pass: u32,
}

pub struct TerrainEncodePass<'a> {
    pub target: &'a wgpu::TextureView,
    pub bindings: &'a TerrainGpuBindings,
    pub instances: &'a [TerrainGpuInstance],
    pub frame: FrameConfig,
    pub runs: &'a [(SpriteBlendMode, Range<u32>)],
    pub load: wgpu::LoadOp<wgpu::Color>,
}

#[derive(Clone, Debug)]
pub struct TemporalTerrainBatch {
    pub instances: Vec<TerrainGpuInstance>,
    pub frame: FrameConfig,
    pub runs: Vec<(SpriteBlendMode, Range<u32>)>,
}

#[derive(Clone, Debug)]
pub struct TemporalTerrainSceneBatch {
    pub terrain: Option<TemporalTerrainBatch>,
    pub wall_graffiti: Option<TemporalTerrainBatch>,
    pub lighting: Option<TemporalTerrainBatch>,
    pub lighting_composite: Option<TerrainLayerComposite>,
    pub effects: Option<TemporalTerrainBatch>,
}

pub struct TemporalTerrainSceneInput<'a> {
    pub frames: &'a [FrameSample],
    pub timeline: Timeline,
    pub geometry_timeline: &'a TerrainGeometryTimeline,
    pub style: &'a TerrainPaintStyle,
    pub bindings: &'a BTreeMap<String, TerrainMaskBindings>,
    pub atlas: &'a TextureAtlas,
    pub board: BoardTransform,
    pub configured_views: NonZeroU32,
    pub output_size: [u32; 2],
}

impl TemporalTerrainSceneBatch {
    /// Compile exact terrain animation state for the same frame views carried
    /// by one heterogeneous scene microbatch.
    pub fn compile(input: TemporalTerrainSceneInput<'_>) -> Result<Self> {
        let TemporalTerrainSceneInput {
            frames,
            timeline,
            geometry_timeline,
            style,
            bindings,
            atlas,
            board,
            configured_views,
            output_size,
        } = input;
        if frames.is_empty() || frames.len() > configured_views.get() as usize {
            return Err(Error::Invalid(
                "temporal terrain scene requires one bounded frame microbatch".to_owned(),
            ));
        }
        let mut plans = Vec::with_capacity(frames.len());
        let mut frame_bindings = Vec::with_capacity(frames.len());
        for frame in frames {
            let span = geometry_timeline.span_at(frame.tick).ok_or_else(|| {
                Error::Invalid(format!(
                    "terrain geometry timeline does not cover frame tick {}",
                    frame.tick
                ))
            })?;
            let geometry = geometry_timeline
                .geometries
                .get(&span.fingerprint)
                .ok_or_else(|| {
                    Error::Invalid(format!(
                        "terrain span references missing geometry {}",
                        span.fingerprint
                    ))
                })?;
            let phase_seconds = span.swamp_phase_seconds(*frame, timeline)?;
            let paint = style.frame(geometry, phase_seconds)?;
            plans.push(TerrainDrawPlan::compile(style, &paint, geometry, atlas)?);
            frame_bindings.push(bindings.get(&span.fingerprint).ok_or_else(|| {
                Error::Invalid(format!(
                    "terrain GPU bank lacks geometry {}",
                    span.fingerprint
                ))
            })?);
        }
        let lighting_composite = plans[0].lighting_composite;
        if plans
            .iter()
            .any(|plan| plan.lighting_composite != lighting_composite)
        {
            return Err(Error::Invalid(
                "terrain lighting composite changes within one temporal batch".to_owned(),
            ));
        }
        let views = plans
            .iter()
            .zip(frame_bindings)
            .map(|(plan, bindings)| (plan, bindings, board))
            .collect::<Vec<_>>();
        let compile = |phase| {
            TemporalTerrainBatch::compile_phase(&views, phase, atlas, configured_views, output_size)
        };
        Ok(Self {
            terrain: compile(TerrainDrawPhase::Terrain)?,
            wall_graffiti: compile(TerrainDrawPhase::WallGraffiti)?,
            lighting: compile(TerrainDrawPhase::Lighting)?,
            lighting_composite,
            effects: compile(TerrainDrawPhase::Effects)?,
        })
    }
}

impl TemporalTerrainBatch {
    /// Conservative per-view slot capacity covering any blend-topology union
    /// formed by one bounded temporal batch. Summing the largest possible
    /// number of distinct plan lengths is independent of frame order without
    /// making allocation proportional to the whole replay.
    pub fn topology_slot_capacity_per_view<'a>(
        plans: impl IntoIterator<Item = &'a TerrainDrawPlan>,
        views_per_batch: NonZeroU32,
    ) -> Result<u32> {
        let plans = plans.into_iter().collect::<Vec<_>>();
        let maximum = [
            TerrainDrawPhase::Terrain,
            TerrainDrawPhase::WallGraffiti,
            TerrainDrawPhase::Lighting,
            TerrainDrawPhase::Effects,
        ]
        .into_iter()
        .map(|phase| {
            let mut lengths = plans
                .iter()
                .map(|plan| phase_operations(plan, phase).len())
                .collect::<Vec<_>>();
            lengths.sort_unstable_by(|left, right| right.cmp(left));
            lengths
                .into_iter()
                .take(views_per_batch.get() as usize)
                .try_fold(0usize, |total, length| {
                    total.checked_add(length).ok_or(Error::ArithmeticOverflow)
                })
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .max()
        .unwrap_or(0);
        u32::try_from(maximum).map_err(|_| Error::ArithmeticOverflow)
    }

    pub fn compile_phase(
        views: &[(&TerrainDrawPlan, &TerrainMaskBindings, BoardTransform)],
        phase: TerrainDrawPhase,
        atlas: &TextureAtlas,
        configured_views: NonZeroU32,
        output_size: [u32; 2],
    ) -> Result<Option<Self>> {
        if !(SpritePipeline::MIN_VIEWS_PER_BATCH..=SpritePipeline::MAX_VIEWS_PER_BATCH)
            .contains(&configured_views.get())
            || views.is_empty()
            || views.len() > configured_views.get() as usize
            || output_size.contains(&0)
        {
            return Err(Error::Invalid(
                "terrain temporal batch dimensions are invalid".to_owned(),
            ));
        }
        let plans = views.iter().map(|(plan, _, _)| *plan).collect::<Vec<_>>();
        let topology = merged_phase_blends(&plans, phase);
        if topology.is_empty() {
            return Ok(None);
        }
        let operation_count =
            u32::try_from(topology.len()).map_err(|_| Error::ArithmeticOverflow)?;
        let mut instances = Vec::with_capacity(
            (configured_views.get() as usize)
                .checked_mul(topology.len())
                .ok_or(Error::ArithmeticOverflow)?,
        );
        for (plan, bindings, board) in views {
            let operations = phase_operations(plan, phase);
            let mut next_operation = 0;
            for blend_mode in &topology {
                if operations
                    .get(next_operation)
                    .is_some_and(|operation| operation.blend_mode == *blend_mode)
                {
                    instances.push(TerrainGpuInstance::compile(
                        &operations[next_operation],
                        bindings,
                        atlas,
                        *board,
                    )?);
                    next_operation += 1;
                } else {
                    instances.push(TerrainGpuInstance::zeroed());
                }
            }
            if next_operation != operations.len() {
                return Err(Error::Invalid(
                    "terrain phase blend topology could not be merged".to_owned(),
                ));
            }
        }
        while instances.len()
            < (configured_views.get() as usize)
                .checked_mul(topology.len())
                .ok_or(Error::ArithmeticOverflow)?
        {
            instances.extend((0..topology.len()).map(|_| TerrainGpuInstance::zeroed()));
        }
        Ok(Some(Self {
            instances,
            frame: FrameConfig {
                instances_per_view: operation_count,
                active_views: u32::try_from(views.len()).map_err(|_| Error::ArithmeticOverflow)?,
                output_size: [output_size[0] as f32, output_size[1] as f32],
            },
            runs: blend_mode_runs(&topology)?,
        }))
    }
}

impl TerrainPipeline {
    pub fn create(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        views_per_batch: NonZeroU32,
    ) -> Result<Self> {
        if target_format != PIXI_COLOR_FORMAT {
            return Err(Error::Invalid(
                "terrain pipeline requires Pixi-compatible RGBA8 UNORM output".to_owned(),
            ));
        }
        if !(SpritePipeline::MIN_VIEWS_PER_BATCH..=SpritePipeline::MAX_VIEWS_PER_BATCH)
            .contains(&views_per_batch.get())
        {
            return Err(Error::Invalid(format!(
                "wgpu terrain multiview batches require {} to {} views",
                SpritePipeline::MIN_VIEWS_PER_BATCH,
                SpritePipeline::MAX_VIEWS_PER_BATCH
            )));
        }
        if !device
            .features()
            .contains(SpritePipeline::REQUIRED_FEATURES)
        {
            return Err(Error::Invalid(
                "GPU device lacks required multiview rendering support".to_owned(),
            ));
        }
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("temporal terrain bindings"),
            entries: &[
                storage_binding(0, std::mem::size_of::<TerrainGpuInstance>() as u64),
                uniform_binding(1, std::mem::size_of::<FrameConfig>() as u64),
                texture_binding(2),
                sampler_binding(3),
                texture_binding(4),
                sampler_binding(5),
                texture_binding(6),
                sampler_binding(7),
                texture_binding(8),
                sampler_binding(9),
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("temporal terrain pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("temporal terrain shader"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(TERRAIN_DRAW_SHADER)),
        });
        let normal = create_terrain_pipeline(
            device,
            &layout,
            &shader,
            target_format,
            views_per_batch,
            "temporal normal terrain pipeline",
            wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
        );
        let add = create_terrain_pipeline(
            device,
            &layout,
            &shader,
            target_format,
            views_per_batch,
            "temporal additive terrain pipeline",
            additive_blend(),
        );
        let multiply = create_terrain_pipeline(
            device,
            &layout,
            &shader,
            target_format,
            views_per_batch,
            "temporal multiply terrain pipeline",
            multiply_blend(),
        );
        let screen = create_terrain_pipeline(
            device,
            &layout,
            &shader,
            target_format,
            views_per_batch,
            "temporal screen terrain pipeline",
            screen_blend(),
        );
        Ok(Self {
            bind_group_layout,
            normal,
            add,
            multiply,
            screen,
            views_per_batch,
        })
    }

    pub fn for_blend_mode(&self, blend_mode: SpriteBlendMode) -> &wgpu::RenderPipeline {
        match blend_mode {
            SpriteBlendMode::Normal => &self.normal,
            SpriteBlendMode::Add => &self.add,
            SpriteBlendMode::Multiply => &self.multiply,
            SpriteBlendMode::Screen => &self.screen,
        }
    }

    pub fn create_bindings(
        &self,
        device: &wgpu::Device,
        atlas: &GpuTextureAtlas,
        masks: &GpuTerrainMaskBank,
        walls: &GpuTerrainWallBank,
        blur: &GpuTerrainBlurBank,
        capacity: NonZeroU32,
    ) -> Result<TerrainGpuBindings> {
        validate_bank_extents(
            [masks.width, masks.height],
            [walls.width, walls.height],
            [blur.width, blur.height],
        )?;
        self.create_bindings_inner(
            device,
            atlas,
            masks,
            Some((&walls.view, &walls.sampler, walls.layers)),
            Some((&blur.view, &blur.sampler, blur.layers)),
            capacity,
        )
    }

    pub(crate) fn create_precomposition_bindings(
        &self,
        device: &wgpu::Device,
        atlas: &GpuTextureAtlas,
        masks: &GpuTerrainMaskBank,
        capacity: NonZeroU32,
    ) -> Result<TerrainGpuBindings> {
        self.create_bindings_inner(device, atlas, masks, None, None, capacity)
    }

    fn create_bindings_inner(
        &self,
        device: &wgpu::Device,
        atlas: &GpuTextureAtlas,
        masks: &GpuTerrainMaskBank,
        wall: Option<(&wgpu::TextureView, &wgpu::Sampler, u32)>,
        blur: Option<(&wgpu::TextureView, &wgpu::Sampler, u32)>,
        capacity: NonZeroU32,
    ) -> Result<TerrainGpuBindings> {
        let buffer_size = u64::from(capacity.get())
            .checked_mul(std::mem::size_of::<TerrainGpuInstance>() as u64)
            .ok_or(Error::ArithmeticOverflow)?;
        if buffer_size > u64::from(device.limits().max_storage_buffer_binding_size) {
            return Err(Error::Invalid(
                "terrain instance buffer exceeds the GPU binding limit".to_owned(),
            ));
        }
        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("temporal terrain instances"),
            size: buffer_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let frame_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("temporal terrain frame configuration"),
            size: std::mem::size_of::<FrameConfig>() as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::UNIFORM,
            mapped_at_creation: false,
        });
        let dummy_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("terrain precomposition dummy texture"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: PIXI_COLOR_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let dummy_view = dummy_texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("terrain precomposition dummy view"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            array_layer_count: Some(1),
            ..Default::default()
        });
        let dummy_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("terrain precomposition dummy sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let (wall_view, wall_sampler, wall_layers) =
            wall.unwrap_or((&dummy_view, &dummy_sampler, 0));
        let (blur_view, blur_sampler, blur_layers) =
            blur.unwrap_or((&dummy_view, &dummy_sampler, 0));
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("temporal terrain bind group"),
            layout: &self.bind_group_layout,
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
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&masks.view),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::Sampler(&masks.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: wgpu::BindingResource::TextureView(wall_view),
                },
                wgpu::BindGroupEntry {
                    binding: 7,
                    resource: wgpu::BindingResource::Sampler(wall_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 8,
                    resource: wgpu::BindingResource::TextureView(blur_view),
                },
                wgpu::BindGroupEntry {
                    binding: 9,
                    resource: wgpu::BindingResource::Sampler(blur_sampler),
                },
            ],
        });
        Ok(TerrainGpuBindings {
            bind_group,
            instance_buffer,
            frame_buffer,
            capacity: capacity.get(),
            atlas_layers: atlas.layers,
            mask_layers: masks.layers,
            wall_layers,
            blur_layers,
        })
    }

    pub fn encode(
        &self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        uploads: &mut TerrainCommandUploads,
        parameters: TerrainEncodePass<'_>,
    ) -> Result<()> {
        let TerrainEncodePass {
            target,
            bindings,
            instances,
            frame,
            runs,
            load,
        } = parameters;
        if frame.active_views > self.views_per_batch.get() {
            return Err(Error::Invalid(
                "active terrain views exceed the configured multiview batch".to_owned(),
            ));
        }
        let expected = frame
            .instances_per_view
            .checked_mul(self.views_per_batch.get())
            .ok_or(Error::ArithmeticOverflow)?;
        if usize::try_from(expected).ok() != Some(instances.len())
            || expected > bindings.capacity
            || expected > uploads.instance_capacity
            || frame.instances_per_view == 0
            || frame.active_views == 0
        {
            return Err(Error::Invalid(
                "terrain temporal instance dimensions are inconsistent".to_owned(),
            ));
        }
        validate_runs(runs, frame.instances_per_view)?;
        let layer_counts = [
            bindings.atlas_layers,
            bindings.mask_layers,
            bindings.wall_layers,
            bindings.blur_layers,
        ];
        for instance in instances {
            validate_instance_layers(instance, layer_counts)?;
        }
        let upload_slot = uploads.next_pass;
        if upload_slot >= uploads.pass_capacity {
            return Err(Error::Invalid(
                "terrain command upload arena has no pass slots remaining".to_owned(),
            ));
        }
        uploads.next_pass += 1;
        let instance_upload_offset = u64::from(upload_slot)
            .checked_mul(uploads.instance_stride)
            .ok_or(Error::ArithmeticOverflow)?;
        let frame_upload_offset = u64::from(upload_slot)
            .checked_mul(std::mem::size_of::<FrameConfig>() as u64)
            .ok_or(Error::ArithmeticOverflow)?;
        let instance_bytes = bytemuck::cast_slice(instances);
        queue.write_buffer(
            &uploads.instance_buffer,
            instance_upload_offset,
            instance_bytes,
        );
        queue.write_buffer(
            &uploads.frame_buffer,
            frame_upload_offset,
            bytemuck::bytes_of(&frame),
        );
        encoder.copy_buffer_to_buffer(
            &uploads.instance_buffer,
            instance_upload_offset,
            &bindings.instance_buffer,
            0,
            instance_bytes.len() as u64,
        );
        encoder.copy_buffer_to_buffer(
            &uploads.frame_buffer,
            frame_upload_offset,
            &bindings.frame_buffer,
            0,
            std::mem::size_of::<FrameConfig>() as u64,
        );
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
            label: Some("temporal terrain phase"),
            color_attachments: &[attachment],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_bind_group(0, &bindings.bind_group, &[]);
        for (blend_mode, range) in runs {
            pass.set_pipeline(self.for_blend_mode(*blend_mode));
            pass.draw(0..6, range.clone());
        }
        Ok(())
    }
}

impl TerrainCommandUploads {
    pub fn create(
        device: &wgpu::Device,
        instance_capacity: NonZeroU32,
        pass_capacity: NonZeroU32,
    ) -> Result<Self> {
        let instance_stride = u64::from(instance_capacity.get())
            .checked_mul(std::mem::size_of::<TerrainGpuInstance>() as u64)
            .ok_or(Error::ArithmeticOverflow)?;
        let instance_bytes = instance_stride
            .checked_mul(u64::from(pass_capacity.get()))
            .ok_or(Error::ArithmeticOverflow)?;
        let frame_bytes = (std::mem::size_of::<FrameConfig>() as u64)
            .checked_mul(u64::from(pass_capacity.get()))
            .ok_or(Error::ArithmeticOverflow)?;
        if instance_bytes > device.limits().max_buffer_size
            || frame_bytes > device.limits().max_buffer_size
        {
            return Err(Error::Invalid(
                "terrain command upload arena exceeds the GPU buffer limit".to_owned(),
            ));
        }
        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("terrain command instance uploads"),
            size: instance_bytes,
            usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let frame_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("terrain command frame uploads"),
            size: frame_bytes,
            usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Ok(Self {
            instance_buffer,
            frame_buffer,
            instance_stride,
            instance_capacity: instance_capacity.get(),
            pass_capacity: pass_capacity.get(),
            next_pass: 0,
        })
    }

    pub fn remaining_passes(&self) -> u32 {
        self.pass_capacity - self.next_pass
    }

    /// Reuse this upload arena after the command submission containing all of
    /// its recorded copies has completed on the GPU.
    ///
    /// Callers must not reset while any submission referencing these upload
    /// ranges remains in flight. A blocking readback of that submission is a
    /// sufficient completion boundary.
    pub fn reset_after_gpu_completion(&mut self) {
        self.next_pass = 0;
    }
}

fn validate_bank_extents(mask: [u32; 2], wall: [u32; 2], blur: [u32; 2]) -> Result<()> {
    if mask.contains(&0) || mask != wall || mask != blur {
        return Err(Error::Invalid(
            "terrain mask, wall, and blur banks must share one positive raster extent".to_owned(),
        ));
    }
    Ok(())
}

fn validate_instance_layers(
    instance: &TerrainGpuInstance,
    [atlas_layers, mask_layers, wall_layers, blur_layers]: [u32; 4],
) -> Result<()> {
    let valid = match instance.source_layers[0] {
        0 => true,
        1 => instance.source_layers[1] < atlas_layers,
        2 => {
            instance.source_layers[2] < mask_layers
                && instance.source_layers[3] < mask_layers
                && (instance.mask_info[2] == NO_TEXTURE_OR_MASK
                    || (instance.mask_info[2] < atlas_layers
                        && instance.mask_info[3] < mask_layers))
        }
        3 => instance.source_layers[2] < mask_layers,
        4 => instance.source_layers[2] < wall_layers,
        5 => instance.source_layers[2] < blur_layers,
        _ => false,
    } && (instance.mask_info[1] == 0 || instance.mask_info[0] < mask_layers);
    if !valid {
        return Err(Error::Invalid(
            "terrain instance references a layer outside its bound bank".to_owned(),
        ));
    }
    Ok(())
}

fn phase_operations(plan: &TerrainDrawPlan, phase: TerrainDrawPhase) -> &[TerrainDrawOp] {
    match phase {
        TerrainDrawPhase::Terrain => &plan.terrain,
        TerrainDrawPhase::WallGraffiti => &plan.wall_graffiti,
        TerrainDrawPhase::Lighting => &plan.lighting,
        TerrainDrawPhase::Effects => &plan.effects,
    }
}

fn merged_phase_blends(
    plans: &[&TerrainDrawPlan],
    phase: TerrainDrawPhase,
) -> Vec<SpriteBlendMode> {
    plans.iter().fold(Vec::new(), |topology, plan| {
        merge_blend_sequences(
            &topology,
            &phase_operations(plan, phase)
                .iter()
                .map(|operation| operation.blend_mode)
                .collect::<Vec<_>>(),
        )
    })
}

fn merge_blend_sequences(
    left: &[SpriteBlendMode],
    right: &[SpriteBlendMode],
) -> Vec<SpriteBlendMode> {
    let mut common_suffix = vec![vec![0usize; right.len() + 1]; left.len() + 1];
    for left_index in (0..left.len()).rev() {
        for right_index in (0..right.len()).rev() {
            common_suffix[left_index][right_index] = if left[left_index] == right[right_index] {
                common_suffix[left_index + 1][right_index + 1] + 1
            } else {
                common_suffix[left_index + 1][right_index]
                    .max(common_suffix[left_index][right_index + 1])
            };
        }
    }
    let mut merged = Vec::with_capacity(left.len() + right.len());
    let (mut left_index, mut right_index) = (0, 0);
    while left_index < left.len() && right_index < right.len() {
        if left[left_index] == right[right_index] {
            merged.push(left[left_index]);
            left_index += 1;
            right_index += 1;
        } else if common_suffix[left_index + 1][right_index]
            >= common_suffix[left_index][right_index + 1]
        {
            merged.push(left[left_index]);
            left_index += 1;
        } else {
            merged.push(right[right_index]);
            right_index += 1;
        }
    }
    merged.extend_from_slice(&left[left_index..]);
    merged.extend_from_slice(&right[right_index..]);
    merged
}

fn blend_mode_runs(blend_modes: &[SpriteBlendMode]) -> Result<Vec<(SpriteBlendMode, Range<u32>)>> {
    let mut runs = Vec::<(SpriteBlendMode, Range<u32>)>::new();
    for (index, blend_mode) in blend_modes.iter().copied().enumerate() {
        let index = u32::try_from(index).map_err(|_| Error::ArithmeticOverflow)?;
        if let Some((previous_blend, range)) = runs.last_mut()
            && *previous_blend == blend_mode
        {
            range.end = index.checked_add(1).ok_or(Error::ArithmeticOverflow)?;
        } else {
            runs.push((
                blend_mode,
                index..index.checked_add(1).ok_or(Error::ArithmeticOverflow)?,
            ));
        }
    }
    Ok(runs)
}

fn storage_binding(binding: u32, minimum_size: u64) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: NonZeroU64::new(minimum_size),
        },
        count: None,
    }
}

fn uniform_binding(binding: u32, minimum_size: u64) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: NonZeroU64::new(minimum_size),
        },
        count: None,
    }
}

fn texture_binding(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2Array,
            multisampled: false,
        },
        count: None,
    }
}

fn sampler_binding(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    }
}

fn create_terrain_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    target_format: wgpu::TextureFormat,
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
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fragment_main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: target_format,
                blend: Some(blend),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview: Some(views_per_batch),
        cache: None,
    })
}

fn validate_runs(runs: &[(SpriteBlendMode, Range<u32>)], count: u32) -> Result<()> {
    let mut next = 0;
    for (_, range) in runs {
        if range.start != next || range.start >= range.end || range.end > count {
            return Err(Error::Invalid(
                "terrain draw runs must cover their instances once in order".to_owned(),
            ));
        }
        next = range.end;
    }
    if next != count {
        return Err(Error::Invalid(
            "terrain draw runs do not cover every operation".to_owned(),
        ));
    }
    Ok(())
}

impl TerrainGpuInstance {
    pub fn compile(
        operation: &TerrainDrawOp,
        bindings: &TerrainMaskBindings,
        atlas: &TextureAtlas,
        board: BoardTransform,
    ) -> Result<Self> {
        validate_operation(operation, board)?;
        let origin = board.point([
            f64::from(operation.placement.origin[0]),
            f64::from(operation.placement.origin[1]),
        ]);
        let extent = board.extent([
            f64::from(operation.placement.size[0]),
            f64::from(operation.placement.size[1]),
        ]);
        let mut instance = Self {
            transform_x: [extent[0], 0.0, origin[0], 0.0],
            transform_y: [0.0, extent[1], origin[1], 0.0],
            atlas_uv: [0.0; 4],
            texture_info: [0.0; 4],
            tile_position_size: [
                0.0,
                0.0,
                operation.placement.size[0],
                operation.placement.size[1],
            ],
            alpha_mask: [operation.alpha, 1.0, 0.0, 0.0],
            fill_color: [0.0; 4],
            stroke_color: [0.0; 4],
            tint: [1.0; 4],
            source_layers: [
                0,
                NO_TEXTURE_OR_MASK,
                NO_TEXTURE_OR_MASK,
                NO_TEXTURE_OR_MASK,
            ],
            mask_info: [
                NO_TEXTURE_OR_MASK,
                0,
                NO_TEXTURE_OR_MASK,
                NO_TEXTURE_OR_MASK,
            ],
            additive_uv: [0.0; 4],
            additive_info: [0.0; 4],
            additive_position_alpha: [0.0; 4],
            additive_tint: [1.0; 4],
        };
        match &operation.source {
            TerrainDrawSource::Solid { color } => {
                instance.fill_color = color_vector(*color);
            }
            TerrainDrawSource::Texture(sample) => {
                instance.source_layers[0] = 1;
                set_texture(
                    &mut instance.atlas_uv,
                    &mut instance.texture_info,
                    &mut instance.tile_position_size,
                    &mut instance.source_layers[1],
                    &mut instance.alpha_mask[2],
                    sample,
                    atlas,
                )?;
                instance.tint = color_vector(sample.tint);
                instance.tint[3] = u8::from(sample.mipmap) as f32;
            }
            TerrainDrawSource::StyledCoverage {
                fill,
                stroke,
                fill_color,
                stroke_color,
                tint,
                additive_texture,
            } => {
                let wall_base = operation.phase == TerrainDrawPhase::Terrain
                    && operation.z_index == 2
                    && matches!(fill, TerrainCoverage::WallFill)
                    && matches!(stroke, TerrainCoverage::WallStroke);
                if wall_base {
                    instance.source_layers[0] = 4;
                    instance.source_layers[2] = bindings.wall_texture.ok_or_else(|| {
                        Error::Invalid(
                            "terrain GPU draw lacks the fixed wall precomposition layer".to_owned(),
                        )
                    })?;
                } else {
                    instance.source_layers[0] = 2;
                    instance.source_layers[2] = required_layer(bindings, fill)?;
                    instance.source_layers[3] = required_layer(bindings, stroke)?;
                    instance.fill_color = color_vector(*fill_color);
                    instance.stroke_color = color_vector(*stroke_color);
                    instance.tint = color_vector(*tint);
                    if let Some((sample, coverage, alpha)) = additive_texture {
                        if !alpha.is_finite() || *alpha < 0.0 {
                            return Err(Error::Invalid(
                                "terrain additive texture alpha must be finite and nonnegative"
                                    .to_owned(),
                            ));
                        }
                        set_texture(
                            &mut instance.additive_uv,
                            &mut instance.additive_info,
                            &mut instance.additive_position_alpha,
                            &mut instance.mask_info[2],
                            &mut instance.alpha_mask[3],
                            sample,
                            atlas,
                        )?;
                        instance.additive_position_alpha[2] = *alpha;
                        instance.additive_tint = color_vector(sample.tint);
                        instance.additive_tint[3] = u8::from(sample.mipmap) as f32;
                        instance.mask_info[3] = required_layer(bindings, coverage)?;
                    }
                }
            }
            TerrainDrawSource::Coverage {
                coverage,
                color,
                blur_pixels,
            } => {
                if *blur_pixels != 0.0 {
                    if *color != 0 || !matches!(coverage, TerrainCoverage::WallFill) {
                        return Err(Error::Invalid(
                            "terrain GPU blur source is not the retained wall shadow".to_owned(),
                        ));
                    }
                    instance.source_layers[0] = 5;
                    instance.source_layers[2] = bindings.wall_shadow.ok_or_else(|| {
                        Error::Invalid(
                            "terrain GPU draw lacks the fixed blurred wall-shadow layer".to_owned(),
                        )
                    })?;
                } else {
                    instance.source_layers[0] = 3;
                    instance.source_layers[2] = required_layer(bindings, coverage)?;
                    instance.fill_color = color_vector(*color);
                }
            }
        }
        if let Some((coverage, alpha)) = &operation.mask {
            if !alpha.is_finite() || *alpha < 0.0 {
                return Err(Error::Invalid(
                    "terrain draw mask alpha must be finite and nonnegative".to_owned(),
                ));
            }
            instance.mask_info[0] = required_layer(bindings, coverage)?;
            instance.mask_info[1] = 1;
            instance.alpha_mask[1] = *alpha;
        }
        Ok(instance)
    }
}

fn set_texture(
    uv: &mut [f32; 4],
    info: &mut [f32; 4],
    position_size: &mut [f32; 4],
    page: &mut u32,
    simple_repeat: &mut f32,
    sample: &TerrainTextureSample,
    atlas: &TextureAtlas,
) -> Result<()> {
    let entry = atlas.entries.get(&sample.atlas_name).ok_or_else(|| {
        Error::Invalid(format!(
            "terrain GPU draw references missing atlas texture {}",
            sample.atlas_name
        ))
    })?;
    if !entry.logical_width.is_finite()
        || !entry.logical_height.is_finite()
        || entry.logical_width <= 0.0
        || entry.logical_height <= 0.0
    {
        return Err(Error::Invalid(
            "terrain GPU draw references invalid atlas dimensions".to_owned(),
        ));
    }
    *uv = [entry.u_min, entry.v_min, entry.u_max, entry.v_max];
    info[0] = entry.logical_width;
    info[1] = entry.logical_height;
    if let Some(scale) = sample.tile_scale {
        if scale
            .iter()
            .any(|value| !value.is_finite() || *value == 0.0)
        {
            return Err(Error::Invalid(
                "terrain tile scale must be finite and nonzero".to_owned(),
            ));
        }
        info[2..4].copy_from_slice(&scale);
    }
    if sample.tile_position.iter().any(|value| !value.is_finite()) {
        return Err(Error::Invalid(
            "terrain tile position must be finite".to_owned(),
        ));
    }
    position_size[..2].copy_from_slice(&sample.tile_position);
    *page = entry.page;
    *simple_repeat = u8::from(pixi_uses_simple_tiling(entry)) as f32;
    Ok(())
}

fn pixi_uses_simple_tiling(entry: &crate::AtlasEntry) -> bool {
    fn power_of_two(value: f32) -> bool {
        value.is_finite() && value > 0.0 && value.fract() == 0.0 && (value as u64).is_power_of_two()
    }

    power_of_two(entry.logical_width) && power_of_two(entry.logical_height)
}

fn required_layer(bindings: &TerrainMaskBindings, coverage: &TerrainCoverage) -> Result<u32> {
    bindings.layer(coverage).ok_or_else(|| {
        Error::Invalid(format!(
            "terrain GPU draw lacks resident coverage {coverage:?}"
        ))
    })
}

fn color_vector(color: u32) -> [f32; 4] {
    [
        ((color >> 16) & 0xff) as f32 / 255.0,
        ((color >> 8) & 0xff) as f32 / 255.0,
        (color & 0xff) as f32 / 255.0,
        1.0,
    ]
}

fn validate_operation(operation: &TerrainDrawOp, board: BoardTransform) -> Result<()> {
    if !operation.alpha.is_finite()
        || operation.alpha < 0.0
        || operation
            .placement
            .origin
            .iter()
            .chain(&operation.placement.size)
            .any(|value| !value.is_finite())
        || operation.placement.size.iter().any(|value| *value <= 0.0)
        || !board.zoom.is_finite()
        || board.zoom <= 0.0
        || board
            .position
            .iter()
            .chain(&board.pivot)
            .any(|value| !value.is_finite())
    {
        return Err(Error::Invalid(
            "terrain GPU draw transform and alpha must be finite and valid".to_owned(),
        ));
    }
    Ok(())
}

pub fn validate_terrain_draw_shader() -> Result<()> {
    let module = naga::front::wgsl::parse_str(TERRAIN_DRAW_SHADER)
        .map_err(|error| Error::Invalid(format!("terrain draw WGSL is invalid: {error}")))?;
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .map_err(|error| Error::Invalid(format!("terrain draw WGSL is unsupported: {error:#?}")))?;
    Ok(())
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TerrainMaskBindings {
    pub wall_fill: Option<u32>,
    pub wall_stroke: Option<u32>,
    pub swamp_fill: Option<u32>,
    pub swamp_stroke: Option<u32>,
    pub private_rampart_fills: BTreeMap<String, u32>,
    pub private_rampart_strokes: BTreeMap<String, u32>,
    pub wall_texture: Option<u32>,
    pub wall_shadow: Option<u32>,
}

impl TerrainMaskBindings {
    pub fn layer(&self, coverage: &TerrainCoverage) -> Option<u32> {
        match coverage {
            TerrainCoverage::WallFill => self.wall_fill,
            TerrainCoverage::WallStroke => self.wall_stroke,
            TerrainCoverage::SwampFill => self.swamp_fill,
            TerrainCoverage::SwampStroke => self.swamp_stroke,
            TerrainCoverage::PrivateRampartFill(user) => {
                self.private_rampart_fills.get(user).copied()
            }
            TerrainCoverage::PrivateRampartStroke(user) => {
                self.private_rampart_strokes.get(user).copied()
            }
        }
    }
}

pub struct GpuTerrainMaskBank {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub sampler: wgpu::Sampler,
    pub width: u32,
    pub height: u32,
    pub layers: u32,
    pub mip_levels: u32,
    pub geometries: BTreeMap<String, TerrainMaskBindings>,
}

impl GpuTerrainMaskBank {
    /// Upload every distinct terrain component once. Frames select resident
    /// layers by geometry fingerprint; no mask bytes cross the CPU/GPU
    /// boundary while rendering timestamps from this bank.
    pub fn upload<'a>(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        geometries: impl IntoIterator<Item = (&'a str, &'a TerrainRasterMasks)>,
    ) -> Result<Self> {
        Self::upload_with_budget(device, queue, geometries, DEFAULT_TERRAIN_BANK_BYTE_BUDGET)
    }

    pub fn upload_with_budget<'a>(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        geometries: impl IntoIterator<Item = (&'a str, &'a TerrainRasterMasks)>,
        byte_budget: u64,
    ) -> Result<Self> {
        let inputs = geometries.into_iter().collect::<Vec<_>>();
        let plan = plan_mask_bank(&inputs)?;
        let logical_layers =
            u32::try_from(plan.masks.len()).map_err(|_| Error::ArithmeticOverflow)?;
        let physical_layers = logical_layers.max(1);
        let (physical_width, physical_height, mip_levels) = if logical_layers == 0 {
            (1, 1, 1)
        } else {
            (
                plan.width,
                plan.height,
                plan.width.max(plan.height).ilog2() + 1,
            )
        };
        let limits = device.limits();
        if physical_width > limits.max_texture_dimension_2d
            || physical_height > limits.max_texture_dimension_2d
            || physical_layers > limits.max_texture_array_layers
        {
            return Err(Error::Invalid(
                "terrain mask bank exceeds GPU device limits and must be partitioned".to_owned(),
            ));
        }
        let bank_bytes = mipmapped_bank_bytes(physical_width, physical_height, physical_layers, 1)?;
        if byte_budget == 0 || bank_bytes > byte_budget {
            return Err(Error::Invalid(format!(
                "terrain mask bank needs {bank_bytes} bytes, exceeding its {byte_budget}-byte \
                 budget; partition the replay into geometry windows"
            )));
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("resident terrain coverage bank"),
            size: wgpu::Extent3d {
                width: physical_width,
                height: physical_height,
                depth_or_array_layers: physical_layers,
            },
            mip_level_count: mip_levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: TERRAIN_MASK_FORMAT,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        for layer in 0..physical_layers {
            let mut width = physical_width;
            let mut height = physical_height;
            let mut alpha = if let Some(mask) = plan.masks.get(layer as usize) {
                mask.alpha.clone()
            } else {
                let byte_count = (width as usize)
                    .checked_mul(height as usize)
                    .ok_or(Error::ArithmeticOverflow)?;
                vec![0; byte_count]
            };
            for mip_level in 0..mip_levels {
                queue.write_texture(
                    texture_copy(&texture, layer, mip_level),
                    &alpha,
                    texture_layout(width, height),
                    texture_extent(width, height),
                );
                if mip_level + 1 < mip_levels {
                    alpha = downsample_r8(&alpha, width, height)?;
                    width = (width / 2).max(1);
                    height = (height / 2).max(1);
                }
            }
        }
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("resident terrain coverage bank view"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            array_layer_count: Some(physical_layers),
            ..Default::default()
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("terrain coverage sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Ok(Self {
            texture,
            view,
            sampler,
            width: plan.width,
            height: plan.height,
            layers: logical_layers,
            mip_levels,
            geometries: plan.geometries,
        })
    }
}

fn mipmapped_bank_bytes(width: u32, height: u32, layers: u32, bytes_per_texel: u64) -> Result<u64> {
    let mut width = u64::from(width);
    let mut height = u64::from(height);
    let mut texels = 0u64;
    loop {
        texels = texels
            .checked_add(width.checked_mul(height).ok_or(Error::ArithmeticOverflow)?)
            .ok_or(Error::ArithmeticOverflow)?;
        if width == 1 && height == 1 {
            break;
        }
        width = (width / 2).max(1);
        height = (height / 2).max(1);
    }
    texels
        .checked_mul(u64::from(layers))
        .and_then(|value| value.checked_mul(bytes_per_texel))
        .ok_or(Error::ArithmeticOverflow)
}

struct TerrainMaskBankPlan {
    width: u32,
    height: u32,
    masks: Vec<Arc<TerrainRasterMask>>,
    geometries: BTreeMap<String, TerrainMaskBindings>,
}

#[derive(Default)]
struct VisibleStrokeMasks {
    wall: Option<Arc<TerrainRasterMask>>,
    swamp: Option<Arc<TerrainRasterMask>>,
    private_ramparts: BTreeMap<String, Arc<TerrainRasterMask>>,
}

fn plan_mask_bank(inputs: &[(&str, &TerrainRasterMasks)]) -> Result<TerrainMaskBankPlan> {
    let [width, height] = inputs
        .first()
        .map(|(_, masks)| [masks.width, masks.height])
        .unwrap_or([1, 1]);
    if width == 0 || height == 0 {
        return Err(Error::Invalid(
            "terrain mask bank dimensions must be positive".to_owned(),
        ));
    }
    let mut components = BTreeMap::<String, Arc<TerrainRasterMask>>::new();
    let mut visible_strokes = Vec::with_capacity(inputs.len());
    for (_, masks) in inputs {
        if masks.width != width || masks.height != height {
            return Err(Error::Invalid(
                "terrain mask bank geometries must share one raster extent".to_owned(),
            ));
        }
        for mask in mask_components(masks) {
            validate_mask(mask, width, height)?;
        }
        for mask in fill_components(masks) {
            insert_mask_component(&mut components, Arc::clone(mask))?;
        }
        let derived = derive_visible_strokes(masks)?;
        for mask in derived
            .wall
            .iter()
            .chain(derived.swamp.iter())
            .chain(derived.private_ramparts.values())
        {
            insert_mask_component(&mut components, Arc::clone(mask))?;
        }
        visible_strokes.push(derived);
    }
    let layers = components
        .keys()
        .enumerate()
        .map(|(index, fingerprint)| {
            u32::try_from(index)
                .map(|layer| (fingerprint.clone(), layer))
                .map_err(|_| Error::ArithmeticOverflow)
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let mut geometries = BTreeMap::new();
    for ((fingerprint, masks), visible_strokes) in inputs.iter().zip(&visible_strokes) {
        let bindings = TerrainMaskBindings {
            wall_fill: mask_layer(masks.wall.as_ref(), &layers)?,
            wall_stroke: mask_layer(visible_strokes.wall.as_ref(), &layers)?,
            swamp_fill: mask_layer(masks.swamp.as_ref(), &layers)?,
            swamp_stroke: mask_layer(visible_strokes.swamp.as_ref(), &layers)?,
            private_rampart_fills: mask_layers(&masks.private_ramparts, &layers)?,
            private_rampart_strokes: mask_layers(&visible_strokes.private_ramparts, &layers)?,
            wall_texture: None,
            wall_shadow: None,
        };
        if geometries
            .insert((*fingerprint).to_owned(), bindings.clone())
            .is_some_and(|previous| previous != bindings)
        {
            return Err(Error::Invalid(
                "terrain mask bank repeats a geometry with different bindings".to_owned(),
            ));
        }
    }
    Ok(TerrainMaskBankPlan {
        width,
        height,
        masks: components.into_values().collect(),
        geometries,
    })
}

fn fill_components(masks: &TerrainRasterMasks) -> impl Iterator<Item = &Arc<TerrainRasterMask>> {
    masks
        .wall
        .iter()
        .chain(masks.swamp.iter())
        .chain(masks.private_ramparts.values())
}

fn mask_components(masks: &TerrainRasterMasks) -> impl Iterator<Item = &Arc<TerrainRasterMask>> {
    masks
        .wall
        .iter()
        .chain(masks.wall_stroke.iter())
        .chain(masks.swamp.iter())
        .chain(masks.swamp_stroke.iter())
        .chain(masks.private_ramparts.values())
        .chain(masks.private_rampart_strokes.values())
}

fn insert_mask_component(
    components: &mut BTreeMap<String, Arc<TerrainRasterMask>>,
    mask: Arc<TerrainRasterMask>,
) -> Result<()> {
    match components.entry(mask.fingerprint.clone()) {
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(mask);
        }
        std::collections::btree_map::Entry::Occupied(entry) if entry.get().alpha != mask.alpha => {
            return Err(Error::Invalid(
                "terrain masks collide on one component fingerprint".to_owned(),
            ));
        }
        std::collections::btree_map::Entry::Occupied(_) => {}
    }
    Ok(())
}

fn derive_visible_strokes(masks: &TerrainRasterMasks) -> Result<VisibleStrokeMasks> {
    let private_ramparts = masks
        .private_rampart_strokes
        .iter()
        .map(|(user, stroke)| {
            let fill = masks.private_ramparts.get(user).ok_or_else(|| {
                Error::Invalid(format!(
                    "terrain private rampart stroke for {user} lacks its fill coverage"
                ))
            })?;
            visible_stroke(fill, stroke).map(|mask| (user.clone(), mask))
        })
        .collect::<Result<_>>()?;
    Ok(VisibleStrokeMasks {
        wall: optional_visible_stroke(masks.wall.as_ref(), masks.wall_stroke.as_ref())?,
        swamp: optional_visible_stroke(masks.swamp.as_ref(), masks.swamp_stroke.as_ref())?,
        private_ramparts,
    })
}

fn optional_visible_stroke(
    fill: Option<&Arc<TerrainRasterMask>>,
    stroke: Option<&Arc<TerrainRasterMask>>,
) -> Result<Option<Arc<TerrainRasterMask>>> {
    match (fill, stroke) {
        (Some(fill), Some(stroke)) => visible_stroke(fill, stroke).map(Some),
        (None, None) | (Some(_), None) => Ok(None),
        (None, Some(_)) => Err(Error::Invalid(
            "terrain stroke coverage lacks its fill coverage".to_owned(),
        )),
    }
}

fn visible_stroke(
    fill: &TerrainRasterMask,
    stroke: &TerrainRasterMask,
) -> Result<Arc<TerrainRasterMask>> {
    if fill.width != stroke.width
        || fill.height != stroke.height
        || fill.alpha.len() != stroke.alpha.len()
    {
        return Err(Error::Invalid(
            "terrain fill and stroke coverage extents disagree".to_owned(),
        ));
    }
    let alpha = fill
        .alpha
        .iter()
        .zip(&stroke.alpha)
        .map(|(fill, stroke)| {
            let visible = u32::from(*stroke) * u32::from(255 - *fill);
            ((visible + 127) / 255) as u8
        })
        .collect();
    let mut hasher = Sha256::new();
    hasher.update(b"screeps-arena-visible-stroke-v1");
    hasher.update(fill.fingerprint.as_bytes());
    hasher.update(stroke.fingerprint.as_bytes());
    Ok(Arc::new(TerrainRasterMask {
        width: fill.width,
        height: fill.height,
        fingerprint: format!("{:x}", hasher.finalize()),
        alpha,
    }))
}

fn validate_mask(mask: &TerrainRasterMask, width: u32, height: u32) -> Result<()> {
    let expected_bytes = (width as usize)
        .checked_mul(height as usize)
        .ok_or(Error::ArithmeticOverflow)?;
    if mask.width != width
        || mask.height != height
        || mask.alpha.len() != expected_bytes
        || mask.fingerprint.len() != 64
        || !mask
            .fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(Error::Invalid(
            "terrain mask bank contains an invalid component".to_owned(),
        ));
    }
    Ok(())
}

fn mask_layer(
    mask: Option<&Arc<TerrainRasterMask>>,
    layers: &BTreeMap<String, u32>,
) -> Result<Option<u32>> {
    mask.map(|mask| {
        layers.get(&mask.fingerprint).copied().ok_or_else(|| {
            Error::Invalid("terrain mask component lacks a planned GPU layer".to_owned())
        })
    })
    .transpose()
}

fn mask_layers(
    masks: &BTreeMap<String, Arc<TerrainRasterMask>>,
    layers: &BTreeMap<String, u32>,
) -> Result<BTreeMap<String, u32>> {
    masks
        .iter()
        .map(|(user, mask)| {
            layers
                .get(&mask.fingerprint)
                .copied()
                .ok_or_else(|| {
                    Error::Invalid("terrain mask component lacks a planned GPU layer".to_owned())
                })
                .map(|layer| (user.clone(), layer))
        })
        .collect()
}

fn texture_copy(
    texture: &wgpu::Texture,
    layer: u32,
    mip_level: u32,
) -> wgpu::TexelCopyTextureInfo<'_> {
    wgpu::TexelCopyTextureInfo {
        texture,
        mip_level,
        origin: wgpu::Origin3d {
            x: 0,
            y: 0,
            z: layer,
        },
        aspect: wgpu::TextureAspect::All,
    }
}

const fn texture_layout(width: u32, height: u32) -> wgpu::TexelCopyBufferLayout {
    wgpu::TexelCopyBufferLayout {
        offset: 0,
        bytes_per_row: Some(width),
        rows_per_image: Some(height),
    }
}

const fn texture_extent(width: u32, height: u32) -> wgpu::Extent3d {
    wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::num::NonZeroU32;

    use bytemuck::Zeroable;

    use crate::{
        AtlasEntry, BoardTransform, SpriteBlendMode, TemporalTerrainBatch, TerrainCoverage,
        TerrainDrawOp, TerrainDrawPhase, TerrainDrawSource, TerrainGeometry, TerrainGpuInstance,
        TerrainMaskBindings, TerrainPlacement, TerrainRasterCache, TerrainRasterMask,
        TerrainRasterStyle, TerrainSwampTexture, TerrainTextureSample, TextureAtlas,
        validate_terrain_draw_shader,
    };

    use super::{
        TERRAIN_MASK_FORMAT, downsample_r8, mipmapped_bank_bytes, pixi_uses_simple_tiling,
        plan_mask_bank, validate_bank_extents, validate_instance_layers, visible_stroke,
    };

    fn geometry(swamp_path: &str) -> TerrainGeometry {
        TerrainGeometry {
            room_size: 5,
            view_box: 500,
            wall_path: Some("M 0 0 h 100 v 100 Z".to_owned()),
            swamp_path: Some(swamp_path.to_owned()),
            private_rampart_paths: BTreeMap::new(),
            private_rampart_colors: BTreeMap::new(),
            swamp_texture: TerrainSwampTexture::Animated,
            fingerprint: "ab".repeat(32),
        }
    }

    #[test]
    fn plans_deterministic_deduplicated_resident_layers() {
        let first_geometry = geometry("M 100 100 h 100 v 100 Z");
        let second_geometry = geometry("M 200 200 h 100 v 100 Z");
        let mut cache = TerrainRasterCache::new(None).unwrap();
        let first = cache
            .load_styled(&first_geometry, 16, 16, TerrainRasterStyle::default())
            .unwrap();
        let second = cache
            .load_styled(&second_geometry, 16, 16, TerrainRasterStyle::default())
            .unwrap();
        let plan = plan_mask_bank(&[("one", &first), ("two", &second)]).unwrap();

        assert_eq!(TERRAIN_MASK_FORMAT, wgpu::TextureFormat::R8Unorm);
        assert_eq!(plan.masks.len(), 6);
        assert_eq!(
            plan.geometries["one"].wall_fill,
            plan.geometries["two"].wall_fill
        );
        assert_ne!(
            plan.geometries["one"].layer(&TerrainCoverage::SwampFill),
            plan.geometries["two"].layer(&TerrainCoverage::SwampFill)
        );
    }

    #[test]
    fn terrain_multiview_shader_validates_and_host_layout_matches() {
        validate_terrain_draw_shader().unwrap();
        assert_eq!(std::mem::size_of::<TerrainGpuInstance>(), 240);
        assert_eq!(std::mem::align_of::<TerrainGpuInstance>(), 4);
    }

    #[test]
    fn resident_banks_fail_closed_on_extents_and_layer_indices() {
        validate_bank_extents([1_920, 1_080], [1_920, 1_080], [1_920, 1_080]).unwrap();
        assert!(validate_bank_extents([1_920, 1_080], [1_280, 720], [1_920, 1_080]).is_err());

        let mut instance = TerrainGpuInstance::zeroed();
        instance.source_layers = [4, u32::MAX, 0, u32::MAX];
        assert!(validate_instance_layers(&instance, [1, 1, 0, 0]).is_err());
        validate_instance_layers(&instance, [1, 1, 1, 0]).unwrap();

        instance.source_layers = [2, u32::MAX, 0, 0];
        instance.mask_info[2] = u32::MAX;
        validate_instance_layers(&instance, [1, 1, 0, 0]).unwrap();
        instance.source_layers[3] = 1;
        assert!(validate_instance_layers(&instance, [1, 1, 0, 0]).is_err());
    }

    #[test]
    fn terrain_bank_budget_counts_every_odd_mip_level() {
        assert_eq!(mipmapped_bank_bytes(3, 2, 4, 1).unwrap(), 28);
        assert_eq!(mipmapped_bank_bytes(5, 3, 2, 1).unwrap(), 36);
    }

    #[test]
    fn coverage_mips_box_filter_premultiplied_alpha() {
        assert_eq!(downsample_r8(&[0, 64, 128, 255], 2, 2).unwrap(), vec![112]);
        assert_eq!(
            downsample_r8(&[0, 64, 128, 192, 255, 255], 3, 2).unwrap(),
            vec![149]
        );
    }

    #[test]
    fn styled_strokes_become_linear_visible_paint_contributions() {
        let mask = |fingerprint: &str, alpha| TerrainRasterMask {
            width: 3,
            height: 1,
            fingerprint: fingerprint.repeat(64),
            alpha,
        };
        let fill = mask("a", vec![255, 0, 128]);
        let stroke = mask("b", vec![255, 255, 128]);
        let visible = visible_stroke(&fill, &stroke).unwrap();
        assert_eq!(visible.alpha, vec![0, 255, 64]);
        assert_eq!(visible.fingerprint.len(), 64);
    }

    #[test]
    fn final_wall_and_shadow_draws_require_fixed_precomposition_layers() {
        let atlas = TextureAtlas {
            entries: BTreeMap::new(),
            pages: Vec::new(),
            padding: 1,
        };
        let board = BoardTransform {
            zoom: 0.1,
            position: [0.0; 2],
            pivot: [0.0; 2],
        };
        let wall = TerrainDrawOp {
            phase: TerrainDrawPhase::Terrain,
            z_index: 2,
            placement: TerrainPlacement {
                origin: [-50.0; 2],
                size: [10_000.0; 2],
            },
            source: TerrainDrawSource::StyledCoverage {
                fill: TerrainCoverage::WallFill,
                stroke: TerrainCoverage::WallStroke,
                fill_color: 0,
                stroke_color: 0,
                tint: 0xff_ff_ff,
                additive_texture: None,
            },
            mask: None,
            alpha: 1.0,
            blend_mode: SpriteBlendMode::Normal,
        };
        assert!(
            TerrainGpuInstance::compile(&wall, &TerrainMaskBindings::default(), &atlas, board)
                .is_err()
        );
        let bindings = TerrainMaskBindings {
            wall_texture: Some(4),
            wall_shadow: Some(7),
            ..Default::default()
        };
        let wall_instance = TerrainGpuInstance::compile(&wall, &bindings, &atlas, board).unwrap();
        assert_eq!(wall_instance.source_layers[0], 4);
        assert_eq!(wall_instance.source_layers[2], 4);

        let shadow = TerrainDrawOp {
            phase: TerrainDrawPhase::Lighting,
            z_index: 1,
            source: TerrainDrawSource::Coverage {
                coverage: TerrainCoverage::WallFill,
                color: 0,
                blur_pixels: 12.288,
            },
            blend_mode: SpriteBlendMode::Multiply,
            ..wall
        };
        let shadow_instance =
            TerrainGpuInstance::compile(&shadow, &bindings, &atlas, board).unwrap();
        assert_eq!(shadow_instance.source_layers[0], 5);
        assert_eq!(shadow_instance.source_layers[2], 7);
    }

    #[test]
    fn compiles_masked_tiled_operations_into_resident_gpu_indices() {
        let atlas = TextureAtlas {
            entries: BTreeMap::from([(
                "noise2".to_owned(),
                AtlasEntry {
                    page: 3,
                    x: 0,
                    y: 0,
                    width: 16,
                    height: 32,
                    logical_width: 8.0,
                    logical_height: 16.0,
                    u_min: 0.1,
                    v_min: 0.2,
                    u_max: 0.3,
                    v_max: 0.6,
                },
            )]),
            pages: Vec::new(),
            padding: 1,
        };
        let operation = TerrainDrawOp {
            phase: TerrainDrawPhase::Terrain,
            z_index: 1,
            placement: TerrainPlacement {
                origin: [-50.0, -50.0],
                size: [10_000.0, 10_000.0],
            },
            source: TerrainDrawSource::Texture(TerrainTextureSample {
                atlas_name: "noise2".to_owned(),
                tint: 0x66_ff_00,
                mipmap: true,
                tile_scale: Some([10.0, 10.0]),
                tile_position: [90.0, 90.0],
            }),
            mask: Some((TerrainCoverage::SwampFill, 0.25)),
            alpha: 0.3,
            blend_mode: SpriteBlendMode::Add,
        };
        let bindings = TerrainMaskBindings {
            swamp_fill: Some(7),
            ..Default::default()
        };
        let instance = TerrainGpuInstance::compile(
            &operation,
            &bindings,
            &atlas,
            BoardTransform {
                zoom: 0.05,
                position: [10.0, 20.0],
                pivot: [-50.0, -50.0],
            },
        )
        .unwrap();
        assert_eq!(instance.transform_x, [500.0, 0.0, 10.0, 0.0]);
        assert_eq!(instance.transform_y, [0.0, 500.0, 20.0, 0.0]);
        assert_eq!(instance.texture_info, [8.0, 16.0, 10.0, 10.0]);
        assert_eq!(
            instance.tile_position_size,
            [90.0, 90.0, 10_000.0, 10_000.0]
        );
        assert_eq!(instance.source_layers, [1, 3, u32::MAX, u32::MAX]);
        assert_eq!(instance.mask_info, [7, 1, u32::MAX, u32::MAX]);
        assert_eq!(instance.alpha_mask[..2], [0.3, 0.25]);
        assert_eq!(instance.alpha_mask[2], 1.0);
        assert_eq!(instance.tint[3], 1.0);
    }

    #[test]
    fn pixi_tiling_seams_repeat_only_for_power_of_two_full_frame_textures() {
        let entry = |width: f32, height: f32| AtlasEntry {
            page: 0,
            x: 0,
            y: 0,
            width: width as u32,
            height: height as u32,
            logical_width: width,
            logical_height: height,
            u_min: 0.0,
            v_min: 0.0,
            u_max: 1.0,
            v_max: 1.0,
        };
        assert!(pixi_uses_simple_tiling(&entry(1_024.0, 2_048.0)));
        assert!(!pixi_uses_simple_tiling(&entry(1_025.0, 1_025.0)));
        assert!(!pixi_uses_simple_tiling(&entry(512.5, 512.0)));
    }

    fn solid_plan(blend_mode: SpriteBlendMode) -> crate::TerrainDrawPlan {
        crate::TerrainDrawPlan {
            terrain: vec![TerrainDrawOp {
                phase: TerrainDrawPhase::Terrain,
                z_index: 0,
                placement: TerrainPlacement {
                    origin: [0.0, 0.0],
                    size: [100.0, 100.0],
                },
                source: TerrainDrawSource::Solid { color: 0x11_22_33 },
                mask: None,
                alpha: 1.0,
                blend_mode,
            }],
            wall_graffiti: Vec::new(),
            lighting: Vec::new(),
            lighting_composite: None,
            effects: Vec::new(),
        }
    }

    #[test]
    fn temporal_phase_batch_pads_inactive_multiview_layers_without_changing_runs() {
        let first = solid_plan(SpriteBlendMode::Normal);
        let second = solid_plan(SpriteBlendMode::Normal);
        let bindings = TerrainMaskBindings::default();
        let board = BoardTransform {
            zoom: 1.0,
            position: [0.0, 0.0],
            pivot: [0.0, 0.0],
        };
        let atlas = TextureAtlas {
            entries: BTreeMap::new(),
            pages: Vec::new(),
            padding: 1,
        };
        let batch = TemporalTerrainBatch::compile_phase(
            &[(&first, &bindings, board), (&second, &bindings, board)],
            TerrainDrawPhase::Terrain,
            &atlas,
            NonZeroU32::new(6).unwrap(),
            [640, 480],
        )
        .unwrap()
        .unwrap();
        assert_eq!(batch.frame.instances_per_view, 1);
        assert_eq!(batch.frame.active_views, 2);
        assert_eq!(batch.instances.len(), 6);
        assert_eq!(batch.instances[0].alpha_mask[0], 1.0);
        assert_eq!(batch.instances[1].alpha_mask[0], 1.0);
        assert!(
            batch.instances[2..]
                .iter()
                .all(|item| item.alpha_mask[0] == 0.0)
        );
        assert_eq!(batch.runs, vec![(SpriteBlendMode::Normal, 0..1)]);

        let incompatible = solid_plan(SpriteBlendMode::Add);
        let mixed = TemporalTerrainBatch::compile_phase(
            &[
                (&first, &bindings, board),
                (&incompatible, &bindings, board),
            ],
            TerrainDrawPhase::Terrain,
            &atlas,
            NonZeroU32::new(2).unwrap(),
            [640, 480],
        );
        let mixed = mixed.unwrap().unwrap();
        assert_eq!(mixed.frame.instances_per_view, 2);
        assert_eq!(
            mixed.runs,
            vec![
                (SpriteBlendMode::Normal, 0..1),
                (SpriteBlendMode::Add, 1..2)
            ]
        );
        assert_eq!(
            mixed
                .instances
                .iter()
                .map(|instance| instance.alpha_mask[0])
                .collect::<Vec<_>>(),
            vec![1.0, 0.0, 0.0, 1.0]
        );

        let mut rampart_appears = solid_plan(SpriteBlendMode::Normal);
        let mut rampart = rampart_appears.terrain[0].clone();
        rampart.phase = TerrainDrawPhase::Effects;
        rampart.blend_mode = SpriteBlendMode::Add;
        rampart_appears.effects.push(rampart);
        let effects = TemporalTerrainBatch::compile_phase(
            &[
                (&first, &bindings, board),
                (&rampart_appears, &bindings, board),
            ],
            TerrainDrawPhase::Effects,
            &atlas,
            NonZeroU32::new(2).unwrap(),
            [640, 480],
        )
        .unwrap()
        .unwrap();
        assert_eq!(effects.frame.instances_per_view, 1);
        assert_eq!(effects.runs, vec![(SpriteBlendMode::Add, 0..1)]);
        assert_eq!(effects.instances[0].alpha_mask[0], 0.0);
        assert_eq!(effects.instances[1].alpha_mask[0], 1.0);
        assert_eq!(
            TemporalTerrainBatch::topology_slot_capacity_per_view(
                [&first, &incompatible],
                NonZeroU32::new(2).unwrap(),
            )
            .unwrap(),
            2
        );
        let many_plans = (0..100)
            .map(|_| solid_plan(SpriteBlendMode::Normal))
            .collect::<Vec<_>>();
        assert_eq!(
            TemporalTerrainBatch::topology_slot_capacity_per_view(
                many_plans.iter(),
                NonZeroU32::new(6).unwrap(),
            )
            .unwrap(),
            6
        );
    }
}
