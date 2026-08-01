use std::borrow::Cow;
use std::num::NonZeroU32;

use crate::{
    EncodedTemporalBatch, Error, PIXI_COLOR_FORMAT, Result, SpritePipeline, TemporalTarget,
    gpu::multiply_blend,
};

pub const LAYER_COMPOSITE_SHADER: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) @interpolate(flat) layer: u32,
}

@group(0) @binding(0)
var source: texture_2d_array<f32>;

@vertex
fn vertex_main(
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

@fragment
fn fragment_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return textureLoad(
        source,
        vec2<i32>(input.position.xy),
        i32(input.layer),
        0,
    );
}
"#;

struct LightingCompositeSlot {
    target: TemporalTarget,
    bind_group: wgpu::BindGroup,
}

/// Per-ring-slot intermediate arrays for Pixi's filtered lighting layer.
///
/// Terrain lighting draws into `lighting_target`; `encode_lighting_composite`
/// then multiplies that complete layer over the main scene target.
pub struct TemporalLayerCompositor {
    slots: Vec<LightingCompositeSlot>,
    pipeline: wgpu::RenderPipeline,
    width: u32,
    height: u32,
    layers: NonZeroU32,
}

impl TemporalLayerCompositor {
    pub fn create(
        device: &wgpu::Device,
        width: u32,
        height: u32,
        layers: NonZeroU32,
        slot_count: NonZeroU32,
    ) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(Error::Invalid(
                "layer compositor dimensions must be positive".to_owned(),
            ));
        }
        if !(SpritePipeline::MIN_VIEWS_PER_BATCH..=SpritePipeline::MAX_VIEWS_PER_BATCH)
            .contains(&layers.get())
        {
            return Err(Error::Invalid(format!(
                "layer compositor requires {} to {} multiview layers",
                SpritePipeline::MIN_VIEWS_PER_BATCH,
                SpritePipeline::MAX_VIEWS_PER_BATCH
            )));
        }
        if !device
            .features()
            .contains(SpritePipeline::REQUIRED_FEATURES)
        {
            return Err(Error::Invalid(
                "GPU device lacks required multiview compositor support".to_owned(),
            ));
        }
        let limits = device.limits();
        if width > limits.max_texture_dimension_2d
            || height > limits.max_texture_dimension_2d
            || layers.get() > limits.max_texture_array_layers
        {
            return Err(Error::Invalid(
                "layer compositor target exceeds GPU texture limits".to_owned(),
            ));
        }

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("lighting composite source layout"),
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
            label: Some("lighting composite pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lighting composite shader"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(LAYER_COMPOSITE_SHADER)),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("multiply lighting layer composite"),
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
                    blend: Some(multiply_blend()),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: Some(layers),
            cache: None,
        });

        let mut slots = Vec::with_capacity(slot_count.get() as usize);
        for _ in 0..slot_count.get() {
            let target = TemporalTarget::create(device, width, height, layers, PIXI_COLOR_FORMAT)?;
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("temporal lighting composite binding"),
                layout: &bind_group_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&target.view),
                }],
            });
            slots.push(LightingCompositeSlot { target, bind_group });
        }
        Ok(Self {
            slots,
            pipeline,
            width,
            height,
            layers,
        })
    }

    pub(crate) fn lighting_target(&self, batch: &EncodedTemporalBatch) -> Result<&TemporalTarget> {
        let slot_index = batch.slot_index();
        self.slots
            .get(slot_index)
            .map(|slot| &slot.target)
            .ok_or_else(|| Error::Invalid(format!("invalid compositor slot {slot_index}")))
    }

    pub(crate) fn encode_lighting_composite_into(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        slot_index: usize,
        target: &TemporalTarget,
        load: wgpu::LoadOp<wgpu::Color>,
    ) -> Result<()> {
        if target.width != self.width
            || target.height != self.height
            || target.layers != self.layers
            || target.format != PIXI_COLOR_FORMAT
        {
            return Err(Error::Invalid(
                "lighting composite and scene targets differ".to_owned(),
            ));
        }
        let slot = self
            .slots
            .get(slot_index)
            .ok_or_else(|| Error::Invalid(format!("invalid compositor slot {slot_index}")))?;
        let attachment = Some(wgpu::RenderPassColorAttachment {
            view: &target.view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load,
                store: wgpu::StoreOp::Store,
            },
        });
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("multiply temporal lighting layer"),
            color_attachments: &[attachment],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &slot.bind_group, &[]);
        pass.draw(0..3, 0..1);
        Ok(())
    }
}

pub fn validate_layer_composite_shader() -> Result<()> {
    let module = naga::front::wgsl::parse_str(LAYER_COMPOSITE_SHADER)
        .map_err(|error| Error::Invalid(format!("layer composite WGSL is invalid: {error}")))?;
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .map_err(|error| Error::Invalid(format!("layer composite WGSL is invalid: {error}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_layer_composite_shader;

    #[test]
    fn composite_shader_validates() {
        validate_layer_composite_shader().unwrap();
    }
}
