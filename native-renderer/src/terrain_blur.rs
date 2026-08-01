use std::borrow::Cow;
use std::collections::BTreeMap;
use std::num::NonZeroU64;

use bytemuck::{Pod, Zeroable};

use crate::{
    DEFAULT_TERRAIN_BANK_BYTE_BUDGET, Error, GpuTerrainMaskBank, PIXI_COLOR_FORMAT, Result,
};

pub const TERRAIN_BLUR_SHADER: &str = r#"
struct BlurConfig {
    direction: vec2<f32>,
    source_layer: u32,
    source_alpha: u32,
    clamp_high: vec2<u32>,
}

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) corner: vec2<f32>,
}

@group(0) @binding(0)
var source: texture_2d_array<f32>;

@group(0) @binding(1)
var<uniform> config: BlurConfig;

const QUAD = array<vec2<f32>, 6>(
    vec2<f32>(0.0, 0.0),
    vec2<f32>(1.0, 0.0),
    vec2<f32>(0.0, 1.0),
    vec2<f32>(0.0, 1.0),
    vec2<f32>(1.0, 0.0),
    vec2<f32>(1.0, 1.0),
);

fn source_texel(coordinate: vec2<i32>, extent: vec2<i32>) -> f32 {
    var clamped = max(coordinate, vec2<i32>(0));
    if clamped.x >= extent.x {
        if config.clamp_high.x == 0u {
            return 0.0;
        }
        clamped.x = extent.x - 1;
    }
    if clamped.y >= extent.y {
        if config.clamp_high.y == 0u {
            return 0.0;
        }
        clamped.y = extent.y - 1;
    }
    let value = textureLoad(
        source,
        clamped,
        i32(config.source_layer),
        0,
    );
    return select(value.r, value.a, config.source_alpha != 0u);
}

fn source_linear(pixel: vec2<f32>, extent: vec2<i32>) -> f32 {
    let base = vec2<i32>(floor(pixel));
    let fraction = fract(pixel);
    let top = mix(
        source_texel(base, extent),
        source_texel(base + vec2<i32>(1, 0), extent),
        fraction.x,
    );
    let bottom = mix(
        source_texel(base + vec2<i32>(0, 1), extent),
        source_texel(base + vec2<i32>(1, 1), extent),
        fraction.x,
    );
    return mix(top, bottom, fraction.y);
}

@vertex
fn vertex_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    let corner = QUAD[vertex_index];
    var output: VertexOutput;
    output.position = vec4<f32>(
        corner.x * 2.0 - 1.0,
        1.0 - corner.y * 2.0,
        0.0,
        1.0,
    );
    output.corner = corner;
    return output;
}

@fragment
fn fragment_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let extent = vec2<i32>(textureDimensions(source).xy);
    let center = input.corner * vec2<f32>(extent) - vec2<f32>(0.5);
    let alpha =
        source_linear(center - config.direction * 2.0, extent) * 0.153388
        + source_linear(center - config.direction, extent) * 0.221461
        + source_linear(center, extent) * 0.250301
        + source_linear(center + config.direction, extent) * 0.221461
        + source_linear(center + config.direction * 2.0, extent) * 0.153388;
    return vec4<f32>(0.0, 0.0, 0.0, alpha);
}
"#;

#[derive(Clone, Debug)]
pub struct TerrainBlurRequest {
    pub key: String,
    pub source_layer: u32,
    pub blur_pixels: f32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TerrainBlurBindings {
    pub layers: BTreeMap<String, u32>,
}

pub struct GpuTerrainBlurBank {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub sampler: wgpu::Sampler,
    pub width: u32,
    pub height: u32,
    pub layers: u32,
    pub bindings: TerrainBlurBindings,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BlurConfig {
    direction: [f32; 2],
    source_layer: u32,
    source_alpha: u32,
    clamp_high: [u32; 2],
}

impl GpuTerrainBlurBank {
    pub fn bind_geometry(
        &self,
        key: &str,
        bindings: &mut crate::TerrainMaskBindings,
    ) -> Result<()> {
        bindings.wall_shadow =
            Some(*self.bindings.layers.get(key).ok_or_else(|| {
                Error::Invalid(format!("terrain blur bank lacks geometry {key}"))
            })?);
        Ok(())
    }

    /// Precompute Pixi's quality-4, five-tap separable wall blur once per
    /// distinct `(coverage layer, strength)` pair. All eight intermediate
    /// passes are RGBA8, preserving RenderTexture quantization.
    /// `filter_pool_screen_size` is the retained renderer's physical view:
    /// Pixi keeps that exact NPOT extent but rounds other pooled textures up.
    pub fn create(
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        source_masks: &GpuTerrainMaskBank,
        filter_pool_screen_size: [u32; 2],
        requests: &[TerrainBlurRequest],
    ) -> Result<Self> {
        Self::create_with_budget(
            device,
            encoder,
            source_masks,
            filter_pool_screen_size,
            requests,
            DEFAULT_TERRAIN_BANK_BYTE_BUDGET,
        )
    }

    pub fn create_with_budget(
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        source_masks: &GpuTerrainMaskBank,
        filter_pool_screen_size: [u32; 2],
        requests: &[TerrainBlurRequest],
        byte_budget: u64,
    ) -> Result<Self> {
        let clamp_high = pixi_pool_clamps_high_edges(
            [source_masks.width, source_masks.height],
            filter_pool_screen_size,
        )?;
        let plan = plan_requests(requests, source_masks.layers)?;
        let logical_layers =
            u32::try_from(plan.identities.len()).map_err(|_| Error::ArithmeticOverflow)?;
        let physical_layers = logical_layers.max(1);
        if physical_layers > device.limits().max_texture_array_layers {
            return Err(Error::Invalid(
                "terrain blur bank exceeds the GPU array-layer limit".to_owned(),
            ));
        }
        let (physical_width, physical_height) = if logical_layers == 0 {
            (1, 1)
        } else {
            (source_masks.width, source_masks.height)
        };
        if physical_width > device.limits().max_texture_dimension_2d
            || physical_height > device.limits().max_texture_dimension_2d
        {
            return Err(Error::Invalid(
                "terrain blur bank exceeds the GPU texture limit".to_owned(),
            ));
        }
        let bank_bytes = blur_bank_bytes(physical_width, physical_height, physical_layers)?;
        if byte_budget == 0 || bank_bytes > byte_budget {
            return Err(Error::Invalid(format!(
                "terrain blur bank needs {bank_bytes} bytes, exceeding its {byte_budget}-byte \
                 budget; partition the replay into geometry windows"
            )));
        }
        let texture_descriptor = wgpu::TextureDescriptor {
            label: Some("terrain blur bank"),
            size: wgpu::Extent3d {
                width: physical_width,
                height: physical_height,
                depth_or_array_layers: physical_layers,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: PIXI_COLOR_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        let scratch_descriptor = wgpu::TextureDescriptor {
            label: Some("terrain blur scratch"),
            ..texture_descriptor
        };
        let ping = device.create_texture(&scratch_descriptor);
        let pong = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("terrain blur second scratch"),
            ..scratch_descriptor
        });
        let texture = device.create_texture(&texture_descriptor);
        let ping_view = array_view(&ping, "terrain blur scratch view", physical_layers);
        let pong_view = array_view(&pong, "terrain blur second scratch view", physical_layers);
        let texture_view = array_view(&texture, "terrain blur bank view", physical_layers);
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("terrain blur bindings"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: NonZeroU64::new(std::mem::size_of::<BlurConfig>() as u64),
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("terrain blur pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("terrain blur shader"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(TERRAIN_BLUR_SHADER)),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("terrain blur pipeline"),
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
                    format: PIXI_COLOR_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });

        for (layer, (source_layer, blur_bits)) in plan.identities.iter().enumerate() {
            let layer = u32::try_from(layer).map_err(|_| Error::ArithmeticOverflow)?;
            let strength = f32::from_bits(*blur_bits) / 4.0;
            let ping_target = layer_view(&ping, "terrain blur scratch layer", layer);
            let pong_target = layer_view(&pong, "terrain blur second scratch layer", layer);
            let output_target = layer_view(&texture, "terrain blur output layer", layer);
            let horizontal_mask = blur_bind_group(
                device,
                &bind_group_layout,
                &source_masks.view,
                BlurConfig {
                    direction: [strength, 0.0],
                    source_layer: *source_layer,
                    source_alpha: 0,
                    clamp_high,
                },
            );
            let horizontal_ping = blur_bind_group(
                device,
                &bind_group_layout,
                &ping_view,
                BlurConfig {
                    direction: [strength, 0.0],
                    source_layer: layer,
                    source_alpha: 1,
                    clamp_high,
                },
            );
            let horizontal_pong = blur_bind_group(
                device,
                &bind_group_layout,
                &pong_view,
                BlurConfig {
                    direction: [strength, 0.0],
                    source_layer: layer,
                    source_alpha: 1,
                    clamp_high,
                },
            );
            let vertical_ping = blur_bind_group(
                device,
                &bind_group_layout,
                &ping_view,
                BlurConfig {
                    direction: [0.0, strength],
                    source_layer: layer,
                    source_alpha: 1,
                    clamp_high,
                },
            );
            let vertical_pong = blur_bind_group(
                device,
                &bind_group_layout,
                &pong_view,
                BlurConfig {
                    direction: [0.0, strength],
                    source_layer: layer,
                    source_alpha: 1,
                    clamp_high,
                },
            );
            let vertical_ping_output = blur_bind_group(
                device,
                &bind_group_layout,
                &ping_view,
                BlurConfig {
                    direction: [0.0, strength],
                    source_layer: layer,
                    source_alpha: 1,
                    clamp_high,
                },
            );
            for (source, target) in [
                (&horizontal_mask, &ping_target),
                (&horizontal_ping, &pong_target),
                (&horizontal_pong, &ping_target),
                (&horizontal_ping, &pong_target),
                (&vertical_pong, &ping_target),
                (&vertical_ping, &pong_target),
                (&vertical_pong, &ping_target),
                (&vertical_ping_output, &output_target),
            ] {
                encode_blur_pass(encoder, &pipeline, source, target);
            }
        }
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("terrain blur bank sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        Ok(Self {
            texture,
            view: texture_view,
            sampler,
            width: source_masks.width,
            height: source_masks.height,
            layers: logical_layers,
            bindings: plan.bindings,
        })
    }
}

fn pixi_pool_clamps_high_edges(source: [u32; 2], screen: [u32; 2]) -> Result<[u32; 2]> {
    if source.contains(&0) || screen.contains(&0) {
        return Err(Error::Invalid(
            "terrain blur source and filter-pool screen extents must be positive".to_owned(),
        ));
    }
    let full_screen = source == screen;
    Ok([
        u32::from(full_screen || source[0].is_power_of_two()),
        u32::from(full_screen || source[1].is_power_of_two()),
    ])
}

fn blur_bank_bytes(width: u32, height: u32, layers: u32) -> Result<u64> {
    u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(u64::from(layers)))
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|one_texture| one_texture.checked_mul(3))
        .ok_or(Error::ArithmeticOverflow)
}

struct BlurPlan {
    identities: Vec<(u32, u32)>,
    bindings: TerrainBlurBindings,
}

fn plan_requests(requests: &[TerrainBlurRequest], source_layers: u32) -> Result<BlurPlan> {
    let mut identities = BTreeMap::<(u32, u32), u32>::new();
    let mut bindings = BTreeMap::new();
    for request in requests {
        if request.key.is_empty()
            || request.source_layer >= source_layers
            || !request.blur_pixels.is_finite()
            || request.blur_pixels <= 0.0
        {
            return Err(Error::Invalid("terrain blur request is invalid".to_owned()));
        }
        let identity = (request.source_layer, request.blur_pixels.to_bits());
        let next_layer = u32::try_from(identities.len()).map_err(|_| Error::ArithmeticOverflow)?;
        let layer = *identities.entry(identity).or_insert(next_layer);
        if bindings.insert(request.key.clone(), layer).is_some() {
            return Err(Error::Invalid(format!(
                "terrain blur request repeats key {}",
                request.key
            )));
        }
    }
    let mut ordered = vec![(0, 0); identities.len()];
    for (identity, layer) in identities {
        ordered[layer as usize] = identity;
    }
    Ok(BlurPlan {
        identities: ordered,
        bindings: TerrainBlurBindings { layers: bindings },
    })
}

fn blur_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    source: &wgpu::TextureView,
    config: BlurConfig,
) -> wgpu::BindGroup {
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("terrain blur configuration"),
        size: std::mem::size_of::<BlurConfig>() as u64,
        usage: wgpu::BufferUsages::UNIFORM,
        mapped_at_creation: true,
    });
    buffer
        .slice(..)
        .get_mapped_range_mut()
        .copy_from_slice(bytemuck::bytes_of(&config));
    buffer.unmap();
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("terrain blur bind group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(source),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: buffer.as_entire_binding(),
            },
        ],
    })
}

fn encode_blur_pass(
    encoder: &mut wgpu::CommandEncoder,
    pipeline: &wgpu::RenderPipeline,
    bindings: &wgpu::BindGroup,
    target: &wgpu::TextureView,
) {
    let attachment = Some(wgpu::RenderPassColorAttachment {
        view: target,
        depth_slice: None,
        resolve_target: None,
        ops: wgpu::Operations {
            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
            store: wgpu::StoreOp::Store,
        },
    });
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("terrain blur pass"),
        color_attachments: &[attachment],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    });
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, bindings, &[]);
    pass.draw(0..6, 0..1);
}

fn array_view(texture: &wgpu::Texture, label: &str, layers: u32) -> wgpu::TextureView {
    texture.create_view(&wgpu::TextureViewDescriptor {
        label: Some(label),
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        array_layer_count: Some(layers),
        ..Default::default()
    })
}

fn layer_view(texture: &wgpu::Texture, label: &str, layer: u32) -> wgpu::TextureView {
    texture.create_view(&wgpu::TextureViewDescriptor {
        label: Some(label),
        dimension: Some(wgpu::TextureViewDimension::D2),
        base_array_layer: layer,
        array_layer_count: Some(1),
        ..Default::default()
    })
}

pub fn validate_terrain_blur_shader() -> Result<()> {
    let module = naga::front::wgsl::parse_str(TERRAIN_BLUR_SHADER)
        .map_err(|error| Error::Invalid(format!("terrain blur WGSL is invalid: {error}")))?;
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .map_err(|error| Error::Invalid(format!("terrain blur WGSL is unsupported: {error:#?}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        TerrainBlurRequest, blur_bank_bytes, pixi_pool_clamps_high_edges, plan_requests,
        validate_terrain_blur_shader,
    };

    #[test]
    fn blur_shader_validates_and_requests_deduplicate_exact_sources() {
        validate_terrain_blur_shader().unwrap();
        assert_eq!(std::mem::size_of::<super::BlurConfig>(), 24);
        let plan = plan_requests(
            &[
                TerrainBlurRequest {
                    key: "one".to_owned(),
                    source_layer: 2,
                    blur_pixels: 12.0,
                },
                TerrainBlurRequest {
                    key: "two".to_owned(),
                    source_layer: 2,
                    blur_pixels: 12.0,
                },
                TerrainBlurRequest {
                    key: "three".to_owned(),
                    source_layer: 3,
                    blur_pixels: 12.0,
                },
            ],
            4,
        )
        .unwrap();
        assert_eq!(plan.identities.len(), 2);
        assert_eq!(plan.bindings.layers["one"], plan.bindings.layers["two"]);
        assert_ne!(plan.bindings.layers["one"], plan.bindings.layers["three"]);
        assert_eq!(blur_bank_bytes(1_920, 1_080, 2).unwrap(), 49_766_400);
        assert_eq!(
            pixi_pool_clamps_high_edges([1_920, 1_080], [1_920, 1_080]).unwrap(),
            [1, 1]
        );
        assert_eq!(
            pixi_pool_clamps_high_edges([2_048, 1_080], [1_920, 1_080]).unwrap(),
            [1, 0]
        );
    }

    #[test]
    fn blur_requests_fail_closed() {
        assert!(
            plan_requests(
                &[TerrainBlurRequest {
                    key: "bad".to_owned(),
                    source_layer: 1,
                    blur_pixels: 0.0,
                }],
                1,
            )
            .is_err()
        );
    }
}
