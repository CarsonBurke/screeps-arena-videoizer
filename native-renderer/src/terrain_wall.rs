use std::collections::BTreeMap;
use std::num::NonZeroU32;

use crate::{
    BoardTransform, DEFAULT_TERRAIN_BANK_BYTE_BUDGET, Error, FrameConfig, GpuTerrainMaskBank,
    GpuTextureAtlas, PIXI_COLOR_FORMAT, Result, SpriteBlendMode, TerrainCommandUploads,
    TerrainDrawOp, TerrainDrawPhase, TerrainDrawSource, TerrainEncodePass, TerrainGpuInstance,
    TerrainMaskBindings, TerrainPipeline, TextureAtlas,
};

pub struct TerrainWallRequest<'a> {
    pub key: &'a str,
    pub operation: &'a TerrainDrawOp,
    pub masks: &'a TerrainMaskBindings,
}

pub struct GpuTerrainWallBank {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub sampler: wgpu::Sampler,
    pub width: u32,
    pub height: u32,
    pub layers: u32,
    pub bindings: BTreeMap<String, u32>,
}

#[derive(Clone, Copy)]
struct WallBake {
    base: TerrainGpuInstance,
    noise: Option<TerrainGpuInstance>,
}

impl GpuTerrainWallBank {
    pub fn bind_geometry(&self, key: &str, bindings: &mut TerrainMaskBindings) -> Result<()> {
        bindings.wall_texture =
            Some(*self.bindings.get(key).ok_or_else(|| {
                Error::Invalid(format!("terrain wall bank lacks geometry {key}"))
            })?);
        Ok(())
    }

    /// Bake the retained client's wall base and masked noise into fixed-size,
    /// mipless RGBA8 RenderTexture equivalents. Camera-space draws then only
    /// linearly scale these resident layers.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        atlas_cpu: &TextureAtlas,
        atlas_gpu: &GpuTextureAtlas,
        mask_bank: &GpuTerrainMaskBank,
        width: u32,
        height: u32,
        requests: &[TerrainWallRequest<'_>],
    ) -> Result<Self> {
        Self::create_with_budget(
            device,
            queue,
            encoder,
            atlas_cpu,
            atlas_gpu,
            mask_bank,
            width,
            height,
            requests,
            DEFAULT_TERRAIN_BANK_BYTE_BUDGET,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_with_budget(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        atlas_cpu: &TextureAtlas,
        atlas_gpu: &GpuTextureAtlas,
        mask_bank: &GpuTerrainMaskBank,
        width: u32,
        height: u32,
        requests: &[TerrainWallRequest<'_>],
        byte_budget: u64,
    ) -> Result<Self> {
        if width == 0
            || height == 0
            || width != mask_bank.width
            || height != mask_bank.height
            || width > device.limits().max_texture_dimension_2d
            || height > device.limits().max_texture_dimension_2d
        {
            return Err(Error::Invalid(
                "terrain wall bank dimensions are invalid".to_owned(),
            ));
        }
        let mut identities = BTreeMap::<Vec<u8>, u32>::new();
        let mut bindings = BTreeMap::new();
        let mut unique_bakes = Vec::new();
        for request in requests {
            validate_wall_operation(request.operation)?;
            let bake =
                compile_wall_bake(request.operation, request.masks, atlas_cpu, width, height)?;
            let mut identity = bytemuck::bytes_of(&bake.base).to_vec();
            if let Some(noise) = bake.noise {
                identity.push(1);
                identity.extend_from_slice(bytemuck::bytes_of(&noise));
            } else {
                identity.push(0);
            }
            let next_layer =
                u32::try_from(identities.len()).map_err(|_| Error::ArithmeticOverflow)?;
            let layer = *identities.entry(identity).or_insert_with(|| {
                unique_bakes.push(bake);
                next_layer
            });
            if request.key.is_empty() || bindings.insert(request.key.to_owned(), layer).is_some() {
                return Err(Error::Invalid(
                    "terrain wall bank repeats or omits a request key".to_owned(),
                ));
            }
        }
        let logical_layers =
            u32::try_from(unique_bakes.len()).map_err(|_| Error::ArithmeticOverflow)?;
        let physical_layers = if logical_layers == 0 {
            1
        } else {
            logical_layers.div_ceil(2) * 2
        };
        if physical_layers > device.limits().max_texture_array_layers {
            return Err(Error::Invalid(
                "terrain wall bank exceeds the GPU array-layer limit".to_owned(),
            ));
        }
        let physical_width = if logical_layers == 0 { 1 } else { width };
        let physical_height = if logical_layers == 0 { 1 } else { height };
        let bank_bytes = wall_bank_bytes(physical_width, physical_height, physical_layers)?;
        if byte_budget == 0 || bank_bytes > byte_budget {
            return Err(Error::Invalid(format!(
                "terrain wall bank needs {bank_bytes} bytes, exceeding its {byte_budget}-byte \
                 budget; partition the replay into geometry windows"
            )));
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("fixed terrain wall bank"),
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
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("fixed terrain wall bank view"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            array_layer_count: Some(physical_layers),
            ..Default::default()
        });
        if !unique_bakes.is_empty() {
            let pipeline =
                TerrainPipeline::create(device, PIXI_COLOR_FORMAT, NonZeroU32::new(2).unwrap())?;
            let bindings = pipeline.create_precomposition_bindings(
                device,
                atlas_gpu,
                mask_bank,
                NonZeroU32::new(2).unwrap(),
            )?;
            let pass_count = u32::try_from(unique_bakes.len().div_ceil(2))
                .map_err(|_| Error::ArithmeticOverflow)?
                .checked_mul(2)
                .ok_or(Error::ArithmeticOverflow)?;
            let mut uploads = TerrainCommandUploads::create(
                device,
                NonZeroU32::new(2).unwrap(),
                NonZeroU32::new(pass_count).ok_or(Error::ArithmeticOverflow)?,
            )?;
            for (chunk_index, chunk) in unique_bakes.chunks(2).enumerate() {
                let mut base_instances = chunk.iter().map(|bake| bake.base).collect::<Vec<_>>();
                let mut noise_instances = chunk
                    .iter()
                    .map(|bake| {
                        bake.noise.unwrap_or_else(|| {
                            let mut invisible = bake.base;
                            invisible.alpha_mask[0] = 0.0;
                            invisible
                        })
                    })
                    .collect::<Vec<_>>();
                if base_instances.len() == 1 {
                    let mut padding = base_instances[0];
                    padding.alpha_mask[0] = 0.0;
                    base_instances.push(padding);
                    noise_instances.push(padding);
                }
                let target = texture.create_view(&wgpu::TextureViewDescriptor {
                    label: Some("fixed terrain wall batch view"),
                    dimension: Some(wgpu::TextureViewDimension::D2Array),
                    base_array_layer: u32::try_from(chunk_index)
                        .map_err(|_| Error::ArithmeticOverflow)?
                        * 2,
                    array_layer_count: Some(2),
                    ..Default::default()
                });
                pipeline.encode(
                    queue,
                    encoder,
                    &mut uploads,
                    TerrainEncodePass {
                        target: &target,
                        bindings: &bindings,
                        instances: &base_instances,
                        frame: FrameConfig {
                            instances_per_view: 1,
                            active_views: u32::try_from(chunk.len())
                                .map_err(|_| Error::ArithmeticOverflow)?,
                            output_size: [width as f32, height as f32],
                        },
                        runs: &[(SpriteBlendMode::Normal, 0..1)],
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    },
                )?;
                pipeline.encode(
                    queue,
                    encoder,
                    &mut uploads,
                    TerrainEncodePass {
                        target: &target,
                        bindings: &bindings,
                        instances: &noise_instances,
                        frame: FrameConfig {
                            instances_per_view: 1,
                            active_views: u32::try_from(chunk.len())
                                .map_err(|_| Error::ArithmeticOverflow)?,
                            output_size: [width as f32, height as f32],
                        },
                        runs: &[(SpriteBlendMode::Add, 0..1)],
                        load: wgpu::LoadOp::Load,
                    },
                )?;
            }
        }
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("fixed terrain wall bank sampler"),
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
            view,
            sampler,
            width,
            height,
            layers: logical_layers,
            bindings,
        })
    }
}

fn compile_wall_bake(
    operation: &TerrainDrawOp,
    masks: &TerrainMaskBindings,
    atlas: &TextureAtlas,
    width: u32,
    height: u32,
) -> Result<WallBake> {
    let TerrainDrawSource::StyledCoverage {
        fill,
        stroke,
        fill_color,
        stroke_color,
        tint,
        additive_texture,
    } = &operation.source
    else {
        return Err(Error::Invalid(
            "terrain wall bank source is not styled coverage".to_owned(),
        ));
    };
    let mut base_operation = operation.clone();
    base_operation.phase = TerrainDrawPhase::WallGraffiti;
    base_operation.source = TerrainDrawSource::StyledCoverage {
        fill: fill.clone(),
        stroke: stroke.clone(),
        fill_color: *fill_color,
        stroke_color: *stroke_color,
        tint: *tint,
        additive_texture: None,
    };
    let board = BoardTransform {
        zoom: 1.0,
        position: [0.0; 2],
        pivot: [0.0; 2],
    };
    let mut base = TerrainGpuInstance::compile(&base_operation, masks, atlas, board)?;
    set_fixed_transform(&mut base, width, height);
    let noise = additive_texture
        .as_ref()
        .map(|(sample, coverage, alpha)| {
            let noise_operation = TerrainDrawOp {
                phase: TerrainDrawPhase::WallGraffiti,
                z_index: operation.z_index,
                placement: operation.placement,
                source: TerrainDrawSource::Texture(sample.clone()),
                mask: Some((coverage.clone(), 1.0)),
                alpha: *alpha,
                blend_mode: SpriteBlendMode::Add,
            };
            let mut noise = TerrainGpuInstance::compile(&noise_operation, masks, atlas, board)?;
            set_fixed_noise_transform(&mut noise, width, height);
            Ok::<TerrainGpuInstance, Error>(noise)
        })
        .transpose()?;
    Ok(WallBake { base, noise })
}

fn wall_bank_bytes(width: u32, height: u32, layers: u32) -> Result<u64> {
    u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(u64::from(layers)))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(Error::ArithmeticOverflow)
}

fn set_fixed_transform(instance: &mut TerrainGpuInstance, width: u32, height: u32) {
    instance.transform_x = [width as f32, 0.0, 0.0, 0.0];
    instance.transform_y = [0.0, height as f32, 0.0, 0.0];
}

fn set_fixed_noise_transform(instance: &mut TerrainGpuInstance, width: u32, height: u32) {
    set_fixed_transform(instance, width, height);
    instance.texture_info[3] *= width as f32 / height as f32;
}

fn validate_wall_operation(operation: &TerrainDrawOp) -> Result<()> {
    let valid_source = matches!(
        &operation.source,
        TerrainDrawSource::StyledCoverage {
            fill: crate::TerrainCoverage::WallFill,
            stroke: crate::TerrainCoverage::WallStroke,
            ..
        }
    );
    if operation.phase != TerrainDrawPhase::Terrain
        || operation.z_index != 2
        || operation.blend_mode != SpriteBlendMode::Normal
        || operation.alpha != 1.0
        || operation.mask.is_some()
        || !valid_source
    {
        return Err(Error::Invalid(
            "terrain wall bank request is not the retained wall-base operation".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use bytemuck::Zeroable;

    use crate::{
        SpriteBlendMode, TerrainCoverage, TerrainDrawOp, TerrainDrawPhase, TerrainDrawSource,
        TerrainGpuInstance, TerrainPlacement,
    };

    use super::{set_fixed_noise_transform, validate_wall_operation, wall_bank_bytes};

    #[test]
    fn only_fixed_wall_base_operations_enter_the_precomposition_bank() {
        let operation = TerrainDrawOp {
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
        validate_wall_operation(&operation).unwrap();
        let mut lighting = operation.clone();
        lighting.phase = TerrainDrawPhase::Lighting;
        assert!(validate_wall_operation(&lighting).is_err());

        let mut masked = operation.clone();
        masked.mask = Some((TerrainCoverage::WallFill, 1.0));
        assert!(validate_wall_operation(&masked).is_err());

        let mut translucent = operation;
        translucent.alpha = 0.5;
        assert!(validate_wall_operation(&translucent).is_err());
    }

    #[test]
    fn rectangular_wall_bakes_preserve_pixis_width_based_noise_scale() {
        let mut instance = TerrainGpuInstance::zeroed();
        instance.texture_info[3] = 8.0;
        set_fixed_noise_transform(&mut instance, 1_920, 1_080);
        assert_eq!(instance.texture_info[3], 8.0 * 1_920.0 / 1_080.0);
        assert_eq!(wall_bank_bytes(1_920, 1_080, 2).unwrap(), 16_588_800);
    }
}
