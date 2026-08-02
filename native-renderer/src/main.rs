#![recursion_limit = "256"]

use std::collections::BTreeMap;
use std::env;
use std::io::{self, Read};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use screeps_arena_native_renderer::{
    ActionRuntime, AtlasOptions, BoardTransform, GenericSceneRuntime, RendererPlan, ReplayArtifact,
    ResolvedActivation, ResolvedScene, SceneNodeTemplates, SceneSchedule, TerrainDrawPlan,
    TerrainGeometryCompiler, TerrainPaintStyle, TerrainRasterCache, TerrainRasterStyle,
    TextureAtlas, Timeline, TimelineEvent, procedural_graphics_assets, vector_graphics_programs,
};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("native-renderer: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args_os();
    let executable = arguments
        .next()
        .and_then(|value| {
            std::path::PathBuf::from(value)
                .file_name()
                .map(|name| name.to_owned())
        })
        .unwrap_or_else(|| "native-renderer".into());
    let Some(path) = arguments.next() else {
        return Err(format!(
            "usage: {} <capture.replay-ir.json> [frame-batch-size]",
            executable.to_string_lossy()
        )
        .into());
    };
    let batch_size = arguments
        .next()
        .map(|value| value.to_string_lossy().parse::<u64>())
        .transpose()?
        .unwrap_or(256);
    if arguments.next().is_some() {
        return Err("too many arguments".into());
    }

    let load_started = Instant::now();
    let artifact = if path == "-" {
        let mut bytes = Vec::new();
        io::stdin().lock().read_to_end(&mut bytes)?;
        ReplayArtifact::from_slice(&bytes)?
    } else {
        ReplayArtifact::from_path(path)?
    };
    let load_ms = load_started.elapsed().as_secs_f64() * 1_000.0;
    let timeline = Timeline::from_replay(&artifact.replay)?;
    let renderer_plan = RendererPlan::compile(&artifact.renderer_contract)?;
    let scene_schedule = SceneSchedule::compile(&artifact, &renderer_plan)?;
    let resolved_scene = ResolvedScene::compile(&artifact, &renderer_plan, &scene_schedule)?;
    let vector_programs = vector_graphics_programs(&resolved_scene)?;
    let vector_commands = vector_programs
        .values()
        .try_fold(0usize, |total, program| {
            total
                .checked_add(program.commands.len())
                .ok_or("vector command count overflow")
        })?;
    let vector_program_count = vector_programs.len();
    drop(vector_programs);
    let atlas_started = Instant::now();
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
    let atlas_ms = atlas_started.elapsed().as_secs_f64() * 1_000.0;
    let mut terrain_geometry_ms = None;
    let mut terrain_raster_ms = None;
    let mut terrain_unique_geometries = None;
    let mut terrain_geometry_spans = None;
    let mut terrain_cache_hits = None;
    let mut terrain_memory_hits = None;
    let mut terrain_disk_hits = None;
    let mut terrain_rasterized_components = None;
    let mut terrain_streamed_components = None;
    let mut terrain_component_requests = None;
    let mut terrain_mask_bytes = None;
    let mut terrain_paint_variants = None;
    let mut terrain_rampart_paints = None;
    let mut terrain_draw_operations = None;
    if artifact
        .renderer_contract
        .inventory
        .preprocessors
        .binary_search_by(|value| value.as_str().cmp("terrain"))
        .is_ok()
        && artifact.replay.render_config.0.is_some()
    {
        let geometry_started = Instant::now();
        let compiler = TerrainGeometryCompiler::new(&artifact)?;
        let [terrain_width, terrain_height] = compiler.raster_dimensions();
        let terrain_timeline = compiler.compile_timeline()?;
        terrain_geometry_ms = Some(geometry_started.elapsed().as_secs_f64() * 1_000.0);
        terrain_unique_geometries = Some(terrain_timeline.geometries.len());
        terrain_geometry_spans = Some(terrain_timeline.spans.len());

        let raster_started = Instant::now();
        let terrain_paint = TerrainPaintStyle::compile(&artifact.renderer_contract, terrain_width)?;
        let terrain_style = TerrainRasterStyle::from_contract(&artifact.renderer_contract)?;
        let mut terrain_cache = TerrainRasterCache::new(terrain_cache_directory())?;
        terrain_cache.plan_timeline_styled(
            &terrain_timeline,
            terrain_width,
            terrain_height,
            terrain_style,
        )?;
        let mut rampart_paints = 0usize;
        let mut draw_operations = 0usize;
        for geometry in terrain_timeline.geometries.values() {
            terrain_cache.load_styled(geometry, terrain_width, terrain_height, terrain_style)?;
            let frame_paint = terrain_paint.frame(geometry, 0.0)?;
            let draw_plan =
                TerrainDrawPlan::compile(&terrain_paint, &frame_paint, geometry, &atlas)?;
            rampart_paints = rampart_paints
                .checked_add(terrain_paint.ramparts(geometry).len())
                .ok_or("terrain rampart paint count overflow")?;
            draw_operations = draw_operations
                .checked_add(draw_plan.terrain.len())
                .and_then(|total| total.checked_add(draw_plan.wall_graffiti.len()))
                .and_then(|total| total.checked_add(draw_plan.lighting.len()))
                .and_then(|total| total.checked_add(draw_plan.effects.len()))
                .ok_or("terrain draw operation count overflow")?;
        }
        let stats = terrain_cache.stats();
        terrain_raster_ms = Some(raster_started.elapsed().as_secs_f64() * 1_000.0);
        terrain_cache_hits = Some(stats.memory_hits + stats.disk_hits);
        terrain_memory_hits = Some(stats.memory_hits);
        terrain_disk_hits = Some(stats.disk_hits);
        terrain_rasterized_components = Some(stats.rasterized);
        terrain_streamed_components = Some(stats.streamed);
        terrain_component_requests = Some(stats.component_requests);
        terrain_mask_bytes = Some(stats.peak_resident_bytes);
        terrain_paint_variants = Some(terrain_timeline.geometries.len());
        terrain_rampart_paints = Some(rampart_paints);
        terrain_draw_operations = Some(draw_operations);
    }
    let mut native_action_handles = 0_u64;
    let mut processor_activation_counts = BTreeMap::<&str, u64>::new();
    for activation in &resolved_scene.activations {
        let actions = match activation {
            ResolvedActivation::Processor { kind, actions, .. } => {
                let count = processor_activation_counts
                    .entry(kind.as_str())
                    .or_default();
                *count = count
                    .checked_add(1)
                    .ok_or("processor activation count overflow")?;
                actions
            }
            ResolvedActivation::Action { actions, .. } => actions,
            ResolvedActivation::Object { .. } => continue,
        };
        for action in actions {
            ActionRuntime::from_resolved(action)?;
            native_action_handles = native_action_handles
                .checked_add(1)
                .ok_or("native action handle count overflow")?;
        }
    }
    let scene_nodes = SceneNodeTemplates::compile(&resolved_scene, &atlas)?;
    // This only controls whether the CLI can exercise the generic
    // container/sprite compiler. It is not a fidelity/support claim: exact
    // Pixi layer ordering and dedicated processor adapters are tracked
    // independently.
    let generic_temporal_benchmark_eligible = artifact.replay.render_config.0.is_some()
        && GenericSceneRuntime::unsupported_processor_kinds(&resolved_scene).is_empty();
    let mut temporal_compile_ms = None;
    let mut temporal_frames = None;
    let mut temporal_batches = None;
    let mut temporal_max_instances_per_view = None;
    let mut temporal_max_vectors_per_view = None;
    let mut temporal_max_vector_vertices_per_batch = None;
    let mut temporal_padded_instances = None;
    let mut temporal_padded_vector_instances = None;
    if generic_temporal_benchmark_eligible && let Some(config) = &artifact.replay.render_config.0 {
        let temporal_started = Instant::now();
        let mut runtime =
            GenericSceneRuntime::new(&artifact, &renderer_plan, &resolved_scene, &scene_nodes)?;
        let mut padded_instances = 0_u64;
        let mut padded_vector_instances = 0_u64;
        let stats =
            runtime.visit_temporal_scene_batches(
                timeline,
                BoardTransform::from(&config.board_frame),
                NonZeroU32::new(6).expect("constant is nonzero"),
                |_frames, batch| {
                    padded_instances = padded_instances
                        .checked_add(u64::try_from(batch.sprites.instances.len()).map_err(
                            |_| screeps_arena_native_renderer::Error::ArithmeticOverflow,
                        )?)
                        .ok_or(screeps_arena_native_renderer::Error::ArithmeticOverflow)?;
                    padded_vector_instances = padded_vector_instances
                        .checked_add(u64::try_from(batch.vectors.instances.len()).map_err(
                            |_| screeps_arena_native_renderer::Error::ArithmeticOverflow,
                        )?)
                        .ok_or(screeps_arena_native_renderer::Error::ArithmeticOverflow)?;
                    Ok(())
                },
            )?;
        temporal_compile_ms = Some(temporal_started.elapsed().as_secs_f64() * 1_000.0);
        temporal_frames = Some(stats.frames);
        temporal_batches = Some(stats.batches);
        temporal_max_instances_per_view = Some(stats.max_instances_per_view);
        temporal_max_vectors_per_view = Some(stats.max_vectors_per_view);
        temporal_max_vector_vertices_per_batch = Some(stats.max_vector_vertices_per_batch);
        temporal_padded_instances = Some(padded_instances);
        temporal_padded_vector_instances = Some(padded_vector_instances);
    }
    let event_count = artifact.replay.renderer_graph.columns[0].len();
    let batch_count = timeline.batches(batch_size)?.count();
    let mut apply_tick_events = 0_u64;
    let mut advance_events = 0_u64;
    let mut render_events = 0_u64;
    for event in timeline.events() {
        match event? {
            TimelineEvent::ApplyTick { .. } => apply_tick_events += 1,
            TimelineEvent::Advance(_) => advance_events += 1,
            TimelineEvent::Render(_) => render_events += 1,
        }
    }
    if apply_tick_events != u64::from(artifact.replay.total_ticks) + 1
        || render_events != timeline.frame_count()
    {
        return Err("native timeline event coverage is incomplete".into());
    }
    let summary = serde_json::json!({
        "schema": artifact.replay.schema,
        "version": artifact.replay.version,
        "totalTicks": artifact.replay.total_ticks,
        "renderWidth": artifact.replay.render_config.0.as_ref().map(|config| config.width),
        "renderHeight": artifact.replay.render_config.0.as_ref().map(|config| config.height),
        "entities": artifact.replay.entities.len(),
        "rendererEvents": event_count,
        "frames": timeline.frame_count(),
        "frameBatches": batch_count,
        "applyTickEvents": apply_tick_events,
        "advanceEvents": advance_events,
        "renderEvents": render_events,
        "objectPlans": renderer_plan.objects.len(),
        "processorDefinitions": renderer_plan.processor_definitions,
        "actionDefinitions": renderer_plan.action_definitions,
        "maxFixedTemplateSlots": renderer_plan.max_fixed_template_slots,
        "hasDynamicOutputs": renderer_plan.has_dynamic_outputs,
        "objectIntervals": scene_schedule.objects.len(),
        "processorIntervals": scene_schedule.processors.len(),
        "processorActivationCounts": processor_activation_counts,
        "actionIntervals": scene_schedule.actions.len(),
        "resolvedActivations": resolved_scene.activations.len(),
        "nativeActionHandles": native_action_handles,
        "finalRendererRandomState": resolved_scene.final_random_state,
        "genericSceneNodes": scene_nodes.nodes.len(),
        "vectorPrograms": vector_program_count,
        "vectorCommands": vector_commands,
        "genericTemporalBenchmarkEligible": generic_temporal_benchmark_eligible,
        "temporalCompileMs": temporal_compile_ms,
        "temporalFrames": temporal_frames,
        "temporalBatches": temporal_batches,
        "temporalMaxInstancesPerView": temporal_max_instances_per_view,
        "temporalMaxVectorsPerView": temporal_max_vectors_per_view,
        "temporalMaxVectorVerticesPerBatch": temporal_max_vector_vertices_per_batch,
        "temporalPaddedInstances": temporal_padded_instances,
        "temporalPaddedVectorInstances": temporal_padded_vector_instances,
        "maxConcurrentFixedOutputBudget": scene_schedule.max_concurrent_fixed_output_budget,
        "maxConcurrentDynamicProcessorBudget": scene_schedule.max_concurrent_dynamic_processor_budget,
        "endpointSeconds": timeline.endpoint().as_f64(),
        "atlasEntries": atlas.entries.len(),
        "atlasPages": atlas.pages.len(),
        "atlasBytes": atlas.pages.iter().map(|page| page.rgba.len()).sum::<usize>(),
        "atlasCacheHit": atlas_cache_hit,
        "atlasBuildMs": atlas_ms,
        "terrainGeometryMs": terrain_geometry_ms,
        "terrainRasterMs": terrain_raster_ms,
        "terrainUniqueGeometries": terrain_unique_geometries,
        "terrainGeometrySpans": terrain_geometry_spans,
        "terrainCacheHits": terrain_cache_hits,
        "terrainMemoryHits": terrain_memory_hits,
        "terrainDiskHits": terrain_disk_hits,
        "terrainRasterizedComponents": terrain_rasterized_components,
        "terrainStreamedComponents": terrain_streamed_components,
        "terrainComponentRequests": terrain_component_requests,
        "terrainMaskBytes": terrain_mask_bytes,
        "terrainPaintVariants": terrain_paint_variants,
        "terrainRampartPaints": terrain_rampart_paints,
        "terrainDrawOperations": terrain_draw_operations,
        "validatedLoadMs": load_ms,
    });
    println!("{}", serde_json::to_string(&summary)?);
    Ok(())
}

fn atlas_cache_directory() -> Option<PathBuf> {
    env::var_os("SCREEPS_ARENA_ATLAS_CACHE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .map(|path| path.join("screeps-arena-videoizer/atlas"))
        })
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map(|path| path.join(".cache/screeps-arena-videoizer/atlas"))
        })
}

fn terrain_cache_directory() -> Option<PathBuf> {
    env::var_os("SCREEPS_ARENA_TERRAIN_CACHE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .map(|path| path.join("screeps-arena-videoizer/terrain"))
        })
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map(|path| path.join(".cache/screeps-arena-videoizer/terrain"))
        })
}
