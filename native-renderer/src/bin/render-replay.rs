use std::collections::{BTreeMap, VecDeque};
use std::env;
use std::future::Future;
use std::io::{self, Read};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::{Arc, mpsc::sync_channel};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;
use std::time::Instant;

use screeps_arena_native_renderer::{
    AtlasOptions, BoardTransform, FfmpegAv1Muxer, FfmpegVideoEncoder, GenericSceneRuntime,
    GpuTerrainBlurBank, GpuTerrainMaskBank, GpuTerrainWallBank, GpuTextureAtlas,
    Nv12BatchConverter, Nv12ReadbackBuffer, PIXI_COLOR_FORMAT, PackedNv12Converter, Rational,
    RendererPlan, ReplayArtifact, ResolvedScene, SceneNodeTemplates, SceneSchedule, SpritePipeline,
    TemporalLayerCompositor, TemporalLightingSource, TemporalRenderBatch, TemporalSpriteRenderer,
    TemporalTarget, TemporalTerrainBatch, TemporalTerrainCache, TemporalTerrainSceneBatch,
    TemporalTerrainSceneInput, TerrainBlurRequest, TerrainCommandUploads, TerrainDrawPhase,
    TerrainDrawPlan, TerrainDrawSource, TerrainGeometryCompiler, TerrainGeometryTimeline,
    TerrainMaskBindings, TerrainPaintStyle, TerrainPipeline, TerrainRasterCache,
    TerrainRasterStyle, TerrainWallRequest, TextureAtlas, Timeline, VideoCodec, VideoEncoderConfig,
    VulkanExternalNv12, VulkanNvencConfig, VulkanNvencEncoder, procedural_graphics_assets,
};

const IN_FLIGHT_BATCHES: u32 = 2;
const TERRAIN_PHASES: u32 = 4;
const DIRECT_NVENC_RING_SIZE: usize = 32;

fn main() {
    if let Err(error) = run() {
        eprintln!("render-replay: {error}");
        std::process::exit(1);
    }
}

#[derive(Debug)]
struct Arguments {
    input: PathBuf,
    output: PathBuf,
    codec: VideoCodec,
    direct_av1: bool,
    quality: u8,
    overwrite: bool,
}

impl Arguments {
    fn parse() -> Result<Self, Box<dyn std::error::Error>> {
        let executable = env::args_os()
            .next()
            .and_then(|path| PathBuf::from(path).file_name().map(ToOwned::to_owned))
            .unwrap_or_else(|| "render-replay".into());
        let usage = || {
            format!(
                "usage: {} <capture.replay-ir.json> <output.mp4> \
                 [--h264|--software] [--quality 0..51] [--overwrite]",
                executable.to_string_lossy()
            )
        };
        let mut positional = Vec::new();
        let mut codec = VideoCodec::H264Nvenc;
        let mut direct_av1 = true;
        let mut quality = 18_u8;
        let mut overwrite = false;
        let mut arguments = env::args_os().skip(1);
        while let Some(argument) = arguments.next() {
            match argument.to_str() {
                Some("--h264") => direct_av1 = false,
                Some("--software") => {
                    direct_av1 = false;
                    codec = VideoCodec::H264Software;
                }
                Some("--overwrite") => overwrite = true,
                Some("--quality") => {
                    let value = arguments
                        .next()
                        .ok_or_else(|| "--quality requires a value".to_owned())?;
                    quality = value.to_string_lossy().parse()?;
                    if quality > 51 {
                        return Err("--quality must be between 0 and 51".into());
                    }
                }
                Some(value) if value.starts_with("--") => {
                    return Err(format!("unknown option {value}\n{}", usage()).into());
                }
                _ => positional.push(PathBuf::from(argument)),
            }
        }
        let [input, output] = positional.as_slice() else {
            return Err(usage().into());
        };
        Ok(Self {
            input: input.clone(),
            output: output.clone(),
            codec,
            direct_av1,
            quality,
            overwrite,
        })
    }
}

struct ResidentTerrain {
    timeline: Option<TerrainGeometryTimeline>,
    style: Option<TerrainPaintStyle>,
    bindings_by_geometry: BTreeMap<String, TerrainMaskBindings>,
    pipeline: TerrainPipeline,
    gpu_bindings: screeps_arena_native_renderer::TerrainGpuBindings,
    command_instance_capacity: NonZeroU32,
    _masks: GpuTerrainMaskBank,
    _walls: GpuTerrainWallBank,
    _blur: GpuTerrainBlurBank,
}

struct ResidentTerrainCache {
    fingerprint: String,
    prefix_operations: usize,
    prefix: TemporalTarget,
    _lighting: Option<TemporalTarget>,
    lighting_source: Option<TemporalLightingSource>,
}

impl ResidentTerrain {
    #[allow(clippy::too_many_arguments)]
    fn create(
        artifact: &ReplayArtifact,
        atlas: &TextureAtlas,
        gpu_atlas: &GpuTextureAtlas,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        views: NonZeroU32,
        output_size: [u32; 2],
    ) -> screeps_arena_native_renderer::Result<Self> {
        let has_terrain = artifact
            .renderer_contract
            .inventory
            .preprocessors
            .binary_search_by(|value| value.as_str().cmp("terrain"))
            .is_ok();
        let (timeline, style, raster_masks, plans) = if has_terrain {
            let compiler = TerrainGeometryCompiler::new(artifact)?;
            let [width, height] = compiler.raster_dimensions();
            let timeline = compiler.compile_timeline()?;
            let style = TerrainPaintStyle::compile(&artifact.renderer_contract, width)?;
            let raster_style = TerrainRasterStyle::from_contract(&artifact.renderer_contract)?;
            let mut cache = TerrainRasterCache::new(terrain_cache_directory())?;
            cache.plan_timeline_styled(&timeline, width, height, raster_style)?;
            let raster_masks = timeline
                .geometries
                .iter()
                .map(|(key, geometry)| {
                    cache
                        .load_styled(geometry, width, height, raster_style)
                        .map(|masks| (key.clone(), masks))
                })
                .collect::<screeps_arena_native_renderer::Result<BTreeMap<_, _>>>()?;
            let plans = timeline
                .geometries
                .iter()
                .map(|(key, geometry)| {
                    let paint = style.frame(geometry, 0.0)?;
                    TerrainDrawPlan::compile(&style, &paint, geometry, atlas)
                        .map(|plan| (key.clone(), plan))
                })
                .collect::<screeps_arena_native_renderer::Result<BTreeMap<_, _>>>()?;
            (Some(timeline), Some(style), raster_masks, plans)
        } else {
            (None, None, BTreeMap::new(), BTreeMap::new())
        };

        let masks = GpuTerrainMaskBank::upload(
            device,
            queue,
            raster_masks
                .iter()
                .map(|(key, masks)| (key.as_str(), masks)),
        )?;
        let mut bindings_by_geometry = masks.geometries.clone();
        let wall_keys = plans
            .iter()
            .filter_map(|(key, plan)| wall_operation(plan).map(|_| key.clone()))
            .collect::<Vec<_>>();
        let wall_requests = wall_keys
            .iter()
            .map(|key| {
                Ok(TerrainWallRequest {
                    key,
                    operation: wall_operation(&plans[key]).ok_or_else(|| {
                        screeps_arena_native_renderer::Error::Invalid(format!(
                            "terrain geometry {key} lacks its wall-base operation"
                        ))
                    })?,
                    masks: &bindings_by_geometry[key],
                })
            })
            .collect::<screeps_arena_native_renderer::Result<Vec<_>>>()?;
        let blur_requests = plans
            .iter()
            .filter_map(|(key, plan)| {
                wall_shadow_blur(plan).map(|blur_pixels| {
                    bindings_by_geometry[key]
                        .wall_fill
                        .ok_or_else(|| {
                            screeps_arena_native_renderer::Error::Invalid(format!(
                                "terrain geometry {key} lacks wall-fill coverage"
                            ))
                        })
                        .map(|source_layer| TerrainBlurRequest {
                            key: key.clone(),
                            source_layer,
                            blur_pixels,
                        })
                })
            })
            .collect::<screeps_arena_native_renderer::Result<Vec<_>>>()?;
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("Replay terrain-bank initialization"),
        });
        let walls = GpuTerrainWallBank::create(
            device,
            queue,
            &mut encoder,
            atlas,
            gpu_atlas,
            &masks,
            masks.width,
            masks.height,
            &wall_requests,
        )?;
        drop(wall_requests);
        let blur =
            GpuTerrainBlurBank::create(device, &mut encoder, &masks, output_size, &blur_requests)?;
        queue.submit(Some(encoder.finish()));
        for key in &wall_keys {
            walls.bind_geometry(
                key,
                bindings_by_geometry.get_mut(key).ok_or_else(|| {
                    screeps_arena_native_renderer::Error::Invalid(format!(
                        "terrain mask bank lacks geometry {key}"
                    ))
                })?,
            )?;
        }
        for request in &blur_requests {
            blur.bind_geometry(
                &request.key,
                bindings_by_geometry.get_mut(&request.key).ok_or_else(|| {
                    screeps_arena_native_renderer::Error::Invalid(format!(
                        "terrain mask bank lacks geometry {}",
                        request.key
                    ))
                })?,
            )?;
        }

        let max_operations_per_view =
            TemporalTerrainBatch::topology_slot_capacity_per_view(plans.values(), views)?.max(1);
        let command_instance_capacity = NonZeroU32::new(
            max_operations_per_view
                .checked_mul(views.get())
                .ok_or(screeps_arena_native_renderer::Error::ArithmeticOverflow)?,
        )
        .ok_or(screeps_arena_native_renderer::Error::ArithmeticOverflow)?;
        let pipeline = TerrainPipeline::create(device, PIXI_COLOR_FORMAT, views)?;
        let gpu_bindings = pipeline.create_bindings(
            device,
            gpu_atlas,
            &masks,
            &walls,
            &blur,
            command_instance_capacity,
        )?;
        Ok(Self {
            timeline,
            style,
            bindings_by_geometry,
            pipeline,
            gpu_bindings,
            command_instance_capacity,
            _masks: masks,
            _walls: walls,
            _blur: blur,
        })
    }

    fn compile_batch(
        &self,
        frames: &[screeps_arena_native_renderer::FrameSample],
        timeline: Timeline,
        atlas: &TextureAtlas,
        board: BoardTransform,
        views: NonZeroU32,
        output_size: [u32; 2],
    ) -> screeps_arena_native_renderer::Result<TemporalTerrainSceneBatch> {
        match (&self.timeline, &self.style) {
            (Some(geometry_timeline), Some(style)) => {
                TemporalTerrainSceneBatch::compile(TemporalTerrainSceneInput {
                    frames,
                    timeline,
                    geometry_timeline,
                    style,
                    bindings: &self.bindings_by_geometry,
                    atlas,
                    board,
                    configured_views: views,
                    output_size,
                })
            }
            (None, None) => Ok(TemporalTerrainSceneBatch {
                terrain: None,
                wall_graffiti: None,
                lighting: None,
                lighting_composite: None,
                effects: None,
            }),
            _ => Err(screeps_arena_native_renderer::Error::Invalid(
                "resident terrain state is internally inconsistent".to_owned(),
            )),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn create_static_cache(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        atlas: &TextureAtlas,
        board: BoardTransform,
        views: NonZeroU32,
        output_size: [u32; 2],
        clear_color: wgpu::Color,
        compositor: &TemporalLayerCompositor,
    ) -> screeps_arena_native_renderer::Result<Option<ResidentTerrainCache>> {
        let (Some(timeline), Some(style)) = (&self.timeline, &self.style) else {
            return Ok(None);
        };
        if timeline.spans.len() != 1 {
            return Ok(None);
        }
        let span = &timeline.spans[0];
        let geometry = &timeline.geometries[&span.fingerprint];
        let bindings = &self.bindings_by_geometry[&span.fingerprint];
        let first = TerrainDrawPlan::compile(style, &style.frame(geometry, 0.0)?, geometry, atlas)?;
        let later = TerrainDrawPlan::compile(style, &style.frame(geometry, 1.0)?, geometry, atlas)?;
        if first.terrain.len() != later.terrain.len()
            || first.wall_graffiti != later.wall_graffiti
            || first.lighting != later.lighting
            || first.lighting_composite != later.lighting_composite
            || first.effects != later.effects
        {
            return Ok(None);
        }
        let animated = first
            .terrain
            .iter()
            .zip(&later.terrain)
            .enumerate()
            .filter_map(|(index, (left, right))| (left != right).then_some(index))
            .collect::<Vec<_>>();
        let Some(prefix_operations) = animated.first().copied() else {
            return Ok(None);
        };
        if animated
            .iter()
            .copied()
            .ne(prefix_operations..prefix_operations + animated.len())
            || first.terrain[..prefix_operations] != later.terrain[..prefix_operations]
            || first.terrain[prefix_operations + animated.len()..]
                != later.terrain[prefix_operations + animated.len()..]
        {
            return Ok(None);
        }
        let prefix_plan = TerrainDrawPlan {
            terrain: first.terrain[..prefix_operations].to_vec(),
            wall_graffiti: Vec::new(),
            lighting: Vec::new(),
            lighting_composite: None,
            effects: Vec::new(),
        };
        let repeated_prefix = (0..views.get())
            .map(|_| (&prefix_plan, bindings, board))
            .collect::<Vec<_>>();
        let prefix_batch = TemporalTerrainBatch::compile_phase(
            &repeated_prefix,
            TerrainDrawPhase::Terrain,
            atlas,
            views,
            output_size,
        )?
        .ok_or_else(|| {
            screeps_arena_native_renderer::Error::Invalid(
                "static terrain prefix unexpectedly compiled to no draw".to_owned(),
            )
        })?;
        let repeated_full = (0..views.get())
            .map(|_| (&first, bindings, board))
            .collect::<Vec<_>>();
        let lighting_batch = TemporalTerrainBatch::compile_phase(
            &repeated_full,
            TerrainDrawPhase::Lighting,
            atlas,
            views,
            output_size,
        )?;
        let prefix = TemporalTarget::create(
            device,
            output_size[0],
            output_size[1],
            views,
            PIXI_COLOR_FORMAT,
        )?;
        let lighting = lighting_batch
            .as_ref()
            .map(|_| {
                TemporalTarget::create(
                    device,
                    output_size[0],
                    output_size[1],
                    views,
                    PIXI_COLOR_FORMAT,
                )
            })
            .transpose()?;
        let pass_count = 1 + u32::from(lighting_batch.is_some());
        let mut uploads = TerrainCommandUploads::create(
            device,
            self.command_instance_capacity,
            NonZeroU32::new(pass_count).expect("static cache has at least one terrain pass"),
        )?;
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("static terrain cache initialization"),
        });
        self.pipeline.encode(
            queue,
            &mut encoder,
            &mut uploads,
            screeps_arena_native_renderer::TerrainEncodePass {
                target: &prefix.view,
                bindings: &self.gpu_bindings,
                instances: &prefix_batch.instances,
                frame: prefix_batch.frame,
                runs: &prefix_batch.runs,
                load: wgpu::LoadOp::Clear(clear_color),
            },
        )?;
        if let (Some(batch), Some(target)) = (&lighting_batch, &lighting) {
            self.pipeline.encode(
                queue,
                &mut encoder,
                &mut uploads,
                screeps_arena_native_renderer::TerrainEncodePass {
                    target: &target.view,
                    bindings: &self.gpu_bindings,
                    instances: &batch.instances,
                    frame: batch.frame,
                    runs: &batch.runs,
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                },
            )?;
        }
        queue.submit(Some(encoder.finish()));
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|error| {
                screeps_arena_native_renderer::Error::Invalid(format!(
                    "static terrain cache initialization failed: {error}"
                ))
            })?;
        let lighting_source = lighting
            .as_ref()
            .map(|target| compositor.create_lighting_source(device, target))
            .transpose()?;
        Ok(Some(ResidentTerrainCache {
            fingerprint: span.fingerprint.clone(),
            prefix_operations,
            prefix,
            _lighting: lighting,
            lighting_source,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn compile_cached_batch(
        &self,
        cache: &ResidentTerrainCache,
        frames: &[screeps_arena_native_renderer::FrameSample],
        timeline: Timeline,
        atlas: &TextureAtlas,
        board: BoardTransform,
        views: NonZeroU32,
        output_size: [u32; 2],
    ) -> screeps_arena_native_renderer::Result<Option<TemporalTerrainSceneBatch>> {
        let (Some(geometry_timeline), Some(style)) = (&self.timeline, &self.style) else {
            return Ok(None);
        };
        let mut plans = Vec::with_capacity(frames.len());
        let mut frame_bindings = Vec::with_capacity(frames.len());
        for frame in frames {
            let span = geometry_timeline.span_at(frame.tick).ok_or_else(|| {
                screeps_arena_native_renderer::Error::Invalid(format!(
                    "terrain geometry timeline does not cover frame tick {}",
                    frame.tick
                ))
            })?;
            if span.fingerprint != cache.fingerprint {
                return Ok(None);
            }
            let geometry = &geometry_timeline.geometries[&span.fingerprint];
            let paint = style.frame(geometry, span.swamp_phase_seconds(*frame, timeline)?)?;
            let full = TerrainDrawPlan::compile(style, &paint, geometry, atlas)?;
            if full.terrain.len() < cache.prefix_operations {
                return Ok(None);
            }
            plans.push(TerrainDrawPlan {
                terrain: full.terrain[cache.prefix_operations..].to_vec(),
                wall_graffiti: full.wall_graffiti,
                lighting: Vec::new(),
                lighting_composite: full.lighting_composite,
                effects: full.effects,
            });
            frame_bindings.push(&self.bindings_by_geometry[&span.fingerprint]);
        }
        let lighting_composite = plans[0].lighting_composite;
        let view_plans = plans
            .iter()
            .zip(frame_bindings)
            .map(|(plan, bindings)| (plan, bindings, board))
            .collect::<Vec<_>>();
        let compile = |phase| {
            TemporalTerrainBatch::compile_phase(&view_plans, phase, atlas, views, output_size)
        };
        Ok(Some(TemporalTerrainSceneBatch {
            terrain: compile(TerrainDrawPhase::Terrain)?,
            wall_graffiti: compile(TerrainDrawPhase::WallGraffiti)?,
            lighting: None,
            lighting_composite,
            effects: compile(TerrainDrawPhase::Effects)?,
        }))
    }
}

fn wall_operation(plan: &TerrainDrawPlan) -> Option<&screeps_arena_native_renderer::TerrainDrawOp> {
    plan.terrain.iter().find(|operation| {
        matches!(
            operation.source,
            TerrainDrawSource::StyledCoverage {
                fill: screeps_arena_native_renderer::TerrainCoverage::WallFill,
                stroke: screeps_arena_native_renderer::TerrainCoverage::WallStroke,
                ..
            }
        )
    })
}

fn wall_shadow_blur(plan: &TerrainDrawPlan) -> Option<f32> {
    plan.lighting
        .iter()
        .find_map(|operation| match operation.source {
            TerrainDrawSource::Coverage {
                coverage: screeps_arena_native_renderer::TerrainCoverage::WallFill,
                color: 0,
                blur_pixels,
            } if blur_pixels > 0.0 => Some(blur_pixels),
            _ => None,
        })
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = Arguments::parse()?;
    let total_started = Instant::now();
    let artifact = if arguments.input.as_os_str() == "-" {
        let mut bytes = Vec::new();
        io::stdin().lock().read_to_end(&mut bytes)?;
        ReplayArtifact::from_slice(&bytes)?
    } else {
        ReplayArtifact::from_path(&arguments.input)?
    };
    let render_config = artifact
        .replay
        .render_config
        .0
        .as_ref()
        .ok_or("ReplayIR does not contain a renderConfig")?;
    let output_size = [render_config.width, render_config.height];
    let timeline = Timeline::from_replay(&artifact.replay)?;
    let frames_per_second = Rational::parse_rate(
        artifact
            .replay
            .timeline
            .frames_per_second
            .0
            .as_deref()
            .ok_or("ReplayIR timeline lacks framesPerSecond")?,
        "framesPerSecond",
    )?;
    let renderer_plan = RendererPlan::compile(&artifact.renderer_contract)?;
    let scene_schedule = SceneSchedule::compile(&artifact, &renderer_plan)?;
    let resolved_scene = ResolvedScene::compile(&artifact, &renderer_plan, &scene_schedule)?;
    GenericSceneRuntime::validate_scene_support(&resolved_scene)?;

    let setup_started = Instant::now();
    let atlas_options = AtlasOptions::default();
    let procedural_assets = procedural_graphics_assets(&resolved_scene, atlas_options)?;
    let (atlas, atlas_cache_hit) = match atlas_cache_directory() {
        Some(directory) => TextureAtlas::load_or_build_cached_with_raster_assets(
            &artifact.renderer_contract,
            atlas_options,
            procedural_assets,
            directory,
        )?,
        None => (
            TextureAtlas::build_with_raster_assets(
                &artifact.renderer_contract,
                atlas_options,
                procedural_assets,
            )?,
            false,
        ),
    };
    let scene_nodes = SceneNodeTemplates::compile(&resolved_scene, &atlas)?;

    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..Default::default()
    });
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
    }))?;
    if !adapter
        .features()
        .contains(SpritePipeline::REQUIRED_FEATURES)
    {
        return Err(format!(
            "{} lacks required GPU multiview support",
            adapter.get_info().name
        )
        .into());
    }
    let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("Screeps Arena replay output device"),
        required_features: SpritePipeline::REQUIRED_FEATURES,
        required_limits: adapter.limits(),
        ..Default::default()
    }))?;
    device.push_error_scope(wgpu::ErrorFilter::Validation);
    let gpu_atlas = GpuTextureAtlas::upload(&device, &queue, &atlas)?;
    let slots = NonZeroU32::new(IN_FLIGHT_BATCHES).expect("slot count is nonzero");
    let views = temporal_views(output_size, slots)?;
    let maximum_nodes = NonZeroU32::new(
        u32::try_from(scene_nodes.nodes.len())
            .map_err(|_| "scene node count exceeds u32")?
            .max(1),
    )
    .expect("maximum node count is nonzero");
    let mut renderer = TemporalSpriteRenderer::create(
        &device,
        &gpu_atlas,
        output_size[0],
        output_size[1],
        views,
        maximum_nodes,
        slots,
    )?;
    let vector_meshes = scene_nodes.vector_meshes().collect::<Vec<_>>();
    let vector_pipeline = screeps_arena_native_renderer::VectorPipeline::create(
        &device,
        &renderer,
        output_size,
        views,
        maximum_nodes,
        slots,
        &vector_meshes,
    )?;
    let resident_terrain = ResidentTerrain::create(
        &artifact,
        &atlas,
        &gpu_atlas,
        &device,
        &queue,
        views,
        output_size,
    )?;
    let compositor =
        TemporalLayerCompositor::create(&device, output_size[0], output_size[1], views, slots)?;
    let fallback_converter = if arguments.direct_av1 {
        None
    } else {
        Some(Nv12BatchConverter::create(&device, renderer.target(0)?)?)
    };
    let mut fallback_readback = fallback_converter
        .as_ref()
        .map(|converter| Nv12ReadbackBuffer::create(&device, converter.layout()))
        .transpose()?;
    let packed_converters = if arguments.direct_av1 {
        (0..IN_FLIGHT_BATCHES as usize)
            .map(|slot| PackedNv12Converter::create(&device, renderer.target(slot)?))
            .collect::<screeps_arena_native_renderer::Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let external_targets = if arguments.direct_av1 {
        (0..DIRECT_NVENC_RING_SIZE)
            .map(|_| {
                VulkanExternalNv12::create(&device, output_size[0], output_size[1]).map(Arc::new)
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    if let Some(error) = block_on(device.pop_error_scope()) {
        return Err(format!("GPU pipeline validation failed: {error}").into());
    }

    if arguments.direct_av1 {
        let mut direct_config =
            VulkanNvencConfig::new(output_size[0], output_size[1], frames_per_second);
        direct_config.constant_qp = u32::from(arguments.quality);
        let direct_encoder = VulkanNvencEncoder::new(direct_config)?;
        let mut encoder_ring = direct_encoder.create_ring(external_targets.clone())?;
        let muxer =
            FfmpegAv1Muxer::spawn(&arguments.output, frames_per_second, arguments.overwrite)?;
        let (mux_sender, mux_receiver) = sync_channel::<Option<Vec<u8>>>(64);
        let mux_worker = thread::Builder::new()
            .name("av1-mux-writer".to_owned())
            .spawn(move || {
                let mut muxer = muxer;
                let mut frames = 0_u64;
                let mut write_seconds = 0.0_f64;
                loop {
                    let Some(packet) = mux_receiver.recv().map_err(|_| {
                        screeps_arena_native_renderer::Error::Invalid(
                            "AV1 mux producer stopped before completing the stream".to_owned(),
                        )
                    })?
                    else {
                        break;
                    };
                    let write_started = Instant::now();
                    muxer.write_packet(&packet)?;
                    write_seconds += write_started.elapsed().as_secs_f64();
                    frames = frames
                        .checked_add(1)
                        .ok_or(screeps_arena_native_renderer::Error::ArithmeticOverflow)?;
                }
                let finish_started = Instant::now();
                let stats = muxer.finish(frames)?;
                Ok::<_, screeps_arena_native_renderer::Error>((
                    stats,
                    write_seconds,
                    finish_started.elapsed().as_secs_f64(),
                ))
            })?;
        let board = BoardTransform::from(&render_config.board_frame);
        let clear_color = rgb_clear_color(render_config.background_color);
        let terrain_cache = resident_terrain.create_static_cache(
            &device,
            &queue,
            &atlas,
            board,
            views,
            output_size,
            clear_color,
            &compositor,
        )?;
        let setup_seconds = setup_started.elapsed().as_secs_f64();
        let render_started = Instant::now();
        let mut rendered_frames = 0_u64;
        let mut muxed_frames = 0_u64;
        let mut drain_seconds = 0.0_f64;
        let mut nvenc_drain_seconds = 0.0_f64;
        let mut terrain_compile_seconds = 0.0_f64;
        let mut command_encode_seconds = 0.0_f64;
        let mut gpu_wait_seconds = 0.0_f64;
        let mut nvenc_submit_seconds = 0.0_f64;
        let mut batch_callback_seconds = 0.0_f64;
        let mut runtime =
            GenericSceneRuntime::new(&artifact, &renderer_plan, &resolved_scene, &scene_nodes)?;
        let mut terrain_uploads = (0..IN_FLIGHT_BATCHES)
            .map(|_| {
                TerrainCommandUploads::create(
                    &device,
                    resident_terrain.command_instance_capacity,
                    NonZeroU32::new(TERRAIN_PHASES).expect("terrain phase count is nonzero"),
                )
            })
            .collect::<screeps_arena_native_renderer::Result<Vec<_>>>()?;
        let mut pending_gpu_submissions =
            VecDeque::<(wgpu::SubmissionIndex, Vec<usize>, usize)>::new();
        let mut next_gpu_slot = 0_usize;
        let scene_stats = runtime.visit_temporal_scene_batches(
            timeline,
            board,
            views,
            |frames, scene_batch| {
                let batch_callback_started = Instant::now();
                while encoder_ring.available_slots() < frames.len() {
                    let drain_started = Instant::now();
                    let nvenc_drain_started = Instant::now();
                    let packet = encoder_ring
                        .drain_oldest()
                        .map_err(|error| {
                            screeps_arena_native_renderer::Error::Invalid(error.to_string())
                        })?
                        .ok_or_else(|| {
                            screeps_arena_native_renderer::Error::Invalid(
                                "direct NVENC ring has neither free nor submitted slots".to_owned(),
                            )
                        })?;
                    nvenc_drain_seconds += nvenc_drain_started.elapsed().as_secs_f64();
                    mux_sender.send(Some(packet.data)).map_err(|_| {
                        screeps_arena_native_renderer::Error::Invalid(
                            "AV1 mux worker stopped before rendering completed".to_owned(),
                        )
                    })?;
                    drain_seconds += drain_started.elapsed().as_secs_f64();
                    muxed_frames = muxed_frames
                        .checked_add(1)
                        .ok_or(screeps_arena_native_renderer::Error::ArithmeticOverflow)?;
                }
                let target_slots = (0..frames.len())
                    .map(|_| {
                        encoder_ring.acquire_slot().ok_or_else(|| {
                            screeps_arena_native_renderer::Error::Invalid(
                                "direct NVENC slot accounting exhausted the target ring".to_owned(),
                            )
                        })
                    })
                    .collect::<screeps_arena_native_renderer::Result<Vec<_>>>()?;
                let target_views = target_slots
                    .iter()
                    .map(|slot| external_targets[*slot].view())
                    .collect::<Vec<_>>();
                let terrain_compile_started = Instant::now();
                let terrain_batch = match terrain_cache.as_ref() {
                    Some(cache) => resident_terrain
                        .compile_cached_batch(
                            cache,
                            frames,
                            timeline,
                            &atlas,
                            board,
                            views,
                            output_size,
                        )?
                        .ok_or_else(|| {
                            screeps_arena_native_renderer::Error::Invalid(
                                "terrain changed after the static cache was selected".to_owned(),
                            )
                        })?,
                    None => resident_terrain.compile_batch(
                        frames,
                        timeline,
                        &atlas,
                        board,
                        views,
                        output_size,
                    )?,
                };
                terrain_compile_seconds += terrain_compile_started.elapsed().as_secs_f64();
                let command_encode_started = Instant::now();
                let mut submission = renderer.begin_submission_at(&device, next_gpu_slot)?;
                let encoded = submission.encode_render_batch(
                    &queue,
                    &mut terrain_uploads[next_gpu_slot],
                    TemporalRenderBatch {
                        vector_pipeline: &vector_pipeline,
                        terrain_pipeline: &resident_terrain.pipeline,
                        terrain_bindings: &resident_terrain.gpu_bindings,
                        compositor: &compositor,
                        terrain: &terrain_batch,
                        scene: &scene_batch,
                        lighting_layer_order: renderer_plan.layer_orders.get("lighting").copied(),
                        clear_color,
                        terrain_cache: terrain_cache.as_ref().map(|cache| TemporalTerrainCache {
                            prefix: &cache.prefix,
                            lighting: cache.lighting_source.as_ref(),
                        }),
                    },
                )?;
                submission.encode_packed_nv12(
                    &encoded,
                    &packed_converters[encoded.slot_index()],
                    &target_views,
                )?;
                let submission_index = submission.submit(&queue);
                command_encode_seconds += command_encode_started.elapsed().as_secs_f64();
                pending_gpu_submissions.push_back((submission_index, target_slots, next_gpu_slot));
                next_gpu_slot = (next_gpu_slot + 1) % IN_FLIGHT_BATCHES as usize;

                if pending_gpu_submissions.len() == IN_FLIGHT_BATCHES as usize {
                    let (submission_index, slots, completed_gpu_slot) = pending_gpu_submissions
                        .pop_front()
                        .expect("in-flight GPU queue reached its configured depth");
                    let gpu_wait_started = Instant::now();
                    VulkanExternalNv12::wait_for_submission(&device, submission_index).map_err(
                        |error| {
                            screeps_arena_native_renderer::Error::Invalid(format!(
                                "direct NVENC Vulkan submission failed: {error}"
                            ))
                        },
                    )?;
                    gpu_wait_seconds += gpu_wait_started.elapsed().as_secs_f64();
                    let nvenc_submit_started = Instant::now();
                    for slot in slots {
                        encoder_ring.submit(slot).map_err(|error| {
                            screeps_arena_native_renderer::Error::Invalid(error.to_string())
                        })?;
                        rendered_frames = rendered_frames
                            .checked_add(1)
                            .ok_or(screeps_arena_native_renderer::Error::ArithmeticOverflow)?;
                    }
                    nvenc_submit_seconds += nvenc_submit_started.elapsed().as_secs_f64();
                    terrain_uploads[completed_gpu_slot].reset_after_gpu_completion();
                }
                batch_callback_seconds += batch_callback_started.elapsed().as_secs_f64();
                Ok(())
            },
        )?;
        let scene_visit_seconds = render_started.elapsed().as_secs_f64();
        // Drain already-submitted encoder outputs while the final Vulkan
        // submissions are still running, so end-of-stream has only the last
        // in-flight GPU batches left to flush.
        let overlap_drain_started = Instant::now();
        while let Some(packet) = encoder_ring
            .drain_oldest()
            .map_err(|error| screeps_arena_native_renderer::Error::Invalid(error.to_string()))?
        {
            mux_sender.send(Some(packet.data)).map_err(|_| {
                screeps_arena_native_renderer::Error::Invalid(
                    "AV1 mux worker stopped before rendering completed".to_owned(),
                )
            })?;
            muxed_frames = muxed_frames
                .checked_add(1)
                .ok_or("muxed frame count overflow")?;
        }
        nvenc_drain_seconds += overlap_drain_started.elapsed().as_secs_f64();
        while let Some((submission_index, slots, completed_gpu_slot)) =
            pending_gpu_submissions.pop_front()
        {
            let gpu_wait_started = Instant::now();
            VulkanExternalNv12::wait_for_submission(&device, submission_index).map_err(
                |error| {
                    screeps_arena_native_renderer::Error::Invalid(format!(
                        "direct NVENC Vulkan submission failed: {error}"
                    ))
                },
            )?;
            gpu_wait_seconds += gpu_wait_started.elapsed().as_secs_f64();
            let nvenc_submit_started = Instant::now();
            for slot in slots {
                encoder_ring
                    .submit(slot)
                    .map_err(|error| error.to_string())?;
                rendered_frames = rendered_frames
                    .checked_add(1)
                    .ok_or("rendered frame count overflow")?;
            }
            nvenc_submit_seconds += nvenc_submit_started.elapsed().as_secs_f64();
            terrain_uploads[completed_gpu_slot].reset_after_gpu_completion();
        }
        let final_nvenc_drain_started = Instant::now();
        let final_packets = encoder_ring.finish()?;
        nvenc_drain_seconds += final_nvenc_drain_started.elapsed().as_secs_f64();
        for packet in final_packets {
            mux_sender.send(Some(packet.data)).map_err(|_| {
                screeps_arena_native_renderer::Error::Invalid(
                    "AV1 mux worker stopped before rendering completed".to_owned(),
                )
            })?;
            muxed_frames = muxed_frames
                .checked_add(1)
                .ok_or("muxed frame count overflow")?;
        }
        mux_sender.send(None).map_err(|_| {
            screeps_arena_native_renderer::Error::Invalid(
                "AV1 mux worker stopped before finalization".to_owned(),
            )
        })?;
        drop(mux_sender);
        let (encoder_stats, mux_write_seconds, mux_finish_seconds) = mux_worker
            .join()
            .map_err(|_| io::Error::other("AV1 mux worker panicked"))??;
        if rendered_frames != timeline.frame_count()
            || muxed_frames != timeline.frame_count()
            || scene_stats.frames != timeline.frame_count()
        {
            return Err(format!(
                "rendered {rendered_frames}, encoded {} frames from a {}-frame timeline",
                encoder_stats.frames,
                timeline.frame_count()
            )
            .into());
        }
        let render_seconds = render_started.elapsed().as_secs_f64();
        let adapter_info = adapter.get_info();
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "input": arguments.input,
                "output": arguments.output,
                "adapter": adapter_info.name,
                "codec": "AV1 NVENC direct Vulkan/CUDA",
                "width": output_size[0],
                "height": output_size[1],
                "frames": encoder_stats.frames,
                "encodedBytes": encoder_stats.bytes,
                "viewsPerBatch": views.get(),
                "temporalBatches": scene_stats.batches,
                "encoderRingSize": DIRECT_NVENC_RING_SIZE,
                "atlasCacheHit": atlas_cache_hit,
                "setupSeconds": setup_seconds,
                "renderAndEncodeSeconds": render_seconds,
                "totalSeconds": total_started.elapsed().as_secs_f64(),
                "framesPerSecond": encoder_stats.frames as f64 / render_seconds,
                "drainSeconds": drain_seconds,
                "nvencDrainSeconds": nvenc_drain_seconds,
                "muxWriteSeconds": mux_write_seconds,
                "muxFinishSeconds": mux_finish_seconds,
                "sceneVisitSeconds": scene_visit_seconds,
                "sceneCompileSeconds": scene_visit_seconds - batch_callback_seconds,
                "batchCallbackSeconds": batch_callback_seconds,
                "sceneApplyTickSeconds": scene_stats.apply_tick_seconds,
                "sceneAdvanceSeconds": scene_stats.advance_seconds,
                "sceneSpritePrepareSeconds": scene_stats.sprite_prepare_seconds,
                "sceneVectorPrepareSeconds": scene_stats.vector_prepare_seconds,
                "sceneDisplayCopySeconds": scene_stats.display_copy_seconds,
                "sceneBatchPackSeconds": scene_stats.batch_pack_seconds,
                "sceneApplyEventSeconds": scene_stats.apply_event_seconds,
                "sceneDisplayRebuildSeconds": scene_stats.display_rebuild_seconds,
                "terrainCompileSeconds": terrain_compile_seconds,
                "commandEncodeSeconds": command_encode_seconds,
                "gpuWaitSeconds": gpu_wait_seconds,
                "nvencSubmitSeconds": nvenc_submit_seconds,
                "includesTerrain": resident_terrain.timeline.is_some(),
                "includesNv12Readback": false,
                "zeroCopyEncoder": true,
            }))?
        );
        return Ok(());
    }

    let setup_seconds = setup_started.elapsed().as_secs_f64();
    let converter = fallback_converter
        .as_ref()
        .expect("fallback output creates its NV12 converter");
    let readback = fallback_readback
        .as_mut()
        .expect("fallback output creates its NV12 readback");

    let encoder_config = VideoEncoderConfig {
        width: output_size[0],
        height: output_size[1],
        frames_per_second,
        codec: arguments.codec,
        quality: arguments.quality,
        overwrite: arguments.overwrite,
    };
    let mut encoder = FfmpegVideoEncoder::spawn(&arguments.output, encoder_config)?;
    let board = BoardTransform::from(&render_config.board_frame);
    let clear_color = rgb_clear_color(render_config.background_color);
    let render_started = Instant::now();
    let mut rendered_frames = 0_u64;
    let mut runtime =
        GenericSceneRuntime::new(&artifact, &renderer_plan, &resolved_scene, &scene_nodes)?;
    let mut terrain_uploads = TerrainCommandUploads::create(
        &device,
        resident_terrain.command_instance_capacity,
        NonZeroU32::new(TERRAIN_PHASES).expect("terrain phase count is nonzero"),
    )?;
    let scene_stats =
        runtime.visit_temporal_scene_batches(timeline, board, views, |frames, scene_batch| {
            let terrain_batch = resident_terrain.compile_batch(
                frames,
                timeline,
                &atlas,
                board,
                views,
                output_size,
            )?;
            let mut submission = renderer.begin_submission(&device)?;
            let encoded = submission.encode_render_batch(
                &queue,
                &mut terrain_uploads,
                TemporalRenderBatch {
                    vector_pipeline: &vector_pipeline,
                    terrain_pipeline: &resident_terrain.pipeline,
                    terrain_bindings: &resident_terrain.gpu_bindings,
                    compositor: &compositor,
                    terrain: &terrain_batch,
                    scene: &scene_batch,
                    lighting_layer_order: renderer_plan.layer_orders.get("lighting").copied(),
                    clear_color,
                    terrain_cache: None,
                },
            )?;
            let pending = submission.encode_nv12_readback(&encoded, converter, readback)?;
            let expected_frames = frames.len();
            let mut output_frames = 0_usize;
            submission.submit_and_visit_nv12(&device, &queue, pending, |frame| {
                encoder.write_frame(frame)?;
                output_frames = output_frames
                    .checked_add(1)
                    .ok_or(screeps_arena_native_renderer::Error::ArithmeticOverflow)?;
                rendered_frames = rendered_frames
                    .checked_add(1)
                    .ok_or(screeps_arena_native_renderer::Error::ArithmeticOverflow)?;
                Ok(())
            })?;
            terrain_uploads.reset_after_gpu_completion();
            if output_frames != expected_frames {
                return Err(screeps_arena_native_renderer::Error::Invalid(format!(
                    "GPU readback returned {} frames for a {}-frame temporal batch",
                    output_frames, expected_frames
                )));
            }
            Ok(())
        })?;
    if rendered_frames != timeline.frame_count() || scene_stats.frames != timeline.frame_count() {
        return Err(format!(
            "rendered {rendered_frames} frames from a {}-frame timeline",
            timeline.frame_count()
        )
        .into());
    }
    let encoder_stats = encoder.finish()?;
    let render_seconds = render_started.elapsed().as_secs_f64();
    let adapter_info = adapter.get_info();
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "input": arguments.input,
            "output": arguments.output,
            "adapter": adapter_info.name,
            "codec": format!("{:?}", arguments.codec),
            "width": output_size[0],
            "height": output_size[1],
            "frames": encoder_stats.frames,
            "bytesFedToEncoder": encoder_stats.bytes,
            "viewsPerBatch": views.get(),
            "temporalBatches": scene_stats.batches,
            "atlasCacheHit": atlas_cache_hit,
            "setupSeconds": setup_seconds,
            "renderAndEncodeSeconds": render_seconds,
            "totalSeconds": total_started.elapsed().as_secs_f64(),
            "framesPerSecond": encoder_stats.frames as f64 / render_seconds,
            "includesTerrain": resident_terrain.timeline.is_some(),
            "includesNv12Readback": true,
            "zeroCopyEncoder": false,
        }))?
    );
    Ok(())
}

fn temporal_views(
    output_size: [u32; 2],
    in_flight_batches: NonZeroU32,
) -> Result<NonZeroU32, Box<dyn std::error::Error>> {
    let texture_equivalents = u64::from(in_flight_batches.get())
        .checked_add(6)
        .ok_or("temporal texture count overflow")?;
    let bytes_per_view = u64::from(output_size[0])
        .checked_mul(u64::from(output_size[1]))
        .and_then(|value| value.checked_mul(4))
        .and_then(|value| value.checked_mul(texture_equivalents))
        .ok_or("temporal target byte count overflow")?;
    let budgeted_views = u32::try_from(
        SpritePipeline::MAX_TEMPORAL_COLOR_BYTES
            .checked_div(bytes_per_view)
            .ok_or("temporal target dimensions require zero bytes")?,
    )
    .unwrap_or(u32::MAX)
    .min(SpritePipeline::MAX_VIEWS_PER_BATCH);
    if budgeted_views < SpritePipeline::MIN_VIEWS_PER_BATCH {
        return Err(format!(
            "{}x{} temporal targets exceed the GPU color budget even at {} views",
            output_size[0],
            output_size[1],
            SpritePipeline::MIN_VIEWS_PER_BATCH
        )
        .into());
    }
    Ok(NonZeroU32::new(budgeted_views).expect("validated nonzero temporal view count"))
}

fn rgb_clear_color(color: u32) -> wgpu::Color {
    wgpu::Color {
        r: f64::from((color >> 16) & 0xff) / 255.0,
        g: f64::from((color >> 8) & 0xff) / 255.0,
        b: f64::from(color & 0xff) / 255.0,
        a: 1.0,
    }
}

fn atlas_cache_directory() -> Option<PathBuf> {
    cache_directory("SCREEPS_ARENA_ATLAS_CACHE_DIR", "atlas")
}

fn terrain_cache_directory() -> Option<PathBuf> {
    cache_directory("SCREEPS_ARENA_TERRAIN_CACHE_DIR", "terrain")
}

fn cache_directory(environment_name: &str, component: &str) -> Option<PathBuf> {
    env::var_os(environment_name)
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .map(|path| path.join("screeps-arena-videoizer").join(component))
        })
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map(|path| path.join(".cache/screeps-arena-videoizer").join(component))
        })
}

fn block_on<F: Future>(future: F) -> F::Output {
    struct ThreadWaker(thread::Thread);

    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => thread::park(),
        }
    }
}
