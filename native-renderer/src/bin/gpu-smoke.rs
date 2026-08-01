use std::future::Future;
use std::num::NonZeroU32;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread;

use screeps_arena_native_renderer::{
    Affine2, BoardTransform, GpuTerrainBlurBank, GpuTerrainMaskBank, GpuTerrainWallBank,
    GpuTextureAtlas, LeasedTerrainPhase, Nv12BatchConverter, Nv12ReadbackBuffer, PIXI_COLOR_FORMAT,
    PreparedSpriteInstance, PreparedVector, SceneDisplayEntry, SceneDrawableKind, SpriteBlendMode,
    SpriteInstance, SpritePipeline, TemporalLayerCompositor, TemporalRenderBatch,
    TemporalSceneBatch, TemporalSpriteBatch, TemporalSpriteRenderer, TemporalTerrainBatch,
    TemporalTerrainSceneBatch, TemporalVectorBatch, TerrainCommandUploads, TerrainDrawOp,
    TerrainDrawPhase, TerrainDrawPlan, TerrainDrawSource, TerrainMaskBindings, TerrainPipeline,
    TerrainPlacement, TextureAtlas, TextureAtlasPage, VectorCommand, VectorFillStyle,
    VectorPipeline, VectorProgram, rgba8_to_nv12_reference, tessellate_vector_program,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("gpu-smoke: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
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
            "{} lacks required multiview support",
            adapter.get_info().name
        )
        .into());
    }
    let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("screeps arena GPU smoke device"),
        required_features: SpritePipeline::REQUIRED_FEATURES,
        required_limits: adapter.limits(),
        ..Default::default()
    }))?;

    device.push_error_scope(wgpu::ErrorFilter::Validation);
    let views = NonZeroU32::new(2).expect("constant is nonzero");
    let slots = NonZeroU32::new(2).expect("constant is nonzero");
    let atlas = TextureAtlas {
        entries: Default::default(),
        pages: vec![TextureAtlasPage {
            width: 1,
            height: 1,
            rgba: vec![255; 4],
        }],
        padding: 1,
    };
    let gpu_atlas = GpuTextureAtlas::upload(&device, &queue, &atlas)?;
    let vector_mesh = tessellate_vector_program(&VectorProgram {
        commands: vec![
            VectorCommand::BeginFill(VectorFillStyle {
                color: 0xff_ff_ff,
                alpha: 0.5,
            }),
            VectorCommand::Rect {
                origin: [-8.0, -8.0],
                size: [16.0, 16.0],
            },
            VectorCommand::Rect {
                origin: [-8.0, -8.0],
                size: [16.0, 16.0],
            },
        ],
    })?;
    let mut renderer = TemporalSpriteRenderer::create(
        &device,
        &gpu_atlas,
        64,
        64,
        views,
        NonZeroU32::new(16).expect("constant is nonzero"),
        slots,
    )?;
    let terrain = TerrainPipeline::create(&device, PIXI_COLOR_FORMAT, views)?;
    let terrain_masks = GpuTerrainMaskBank::upload(
        &device,
        &queue,
        std::iter::empty::<(&str, &screeps_arena_native_renderer::TerrainRasterMasks)>(),
    )?;
    let mut terrain_bank_encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("GPU smoke terrain-bank initialization"),
    });
    let terrain_walls = GpuTerrainWallBank::create(
        &device,
        &queue,
        &mut terrain_bank_encoder,
        &atlas,
        &gpu_atlas,
        &terrain_masks,
        1,
        1,
        &[],
    )?;
    let terrain_blur = GpuTerrainBlurBank::create(
        &device,
        &mut terrain_bank_encoder,
        &terrain_masks,
        [64, 64],
        &[],
    )?;
    queue.submit(Some(terrain_bank_encoder.finish()));
    let terrain_bindings = terrain.create_bindings(
        &device,
        &gpu_atlas,
        &terrain_masks,
        &terrain_walls,
        &terrain_blur,
        NonZeroU32::new(2).expect("constant is nonzero"),
    )?;
    let vector_pipeline = VectorPipeline::create(
        &device,
        &renderer,
        [64, 64],
        views,
        NonZeroU32::new(16).expect("constant is nonzero"),
        slots,
        &[&vector_mesh, &vector_mesh],
    )?;
    let compositor = TemporalLayerCompositor::create(&device, 64, 64, views, slots)?;
    let converter = Nv12BatchConverter::create(&device, renderer.target(0)?)?;
    let second_converter = Nv12BatchConverter::create(&device, renderer.target(1)?)?;
    let mut readback = Nv12ReadbackBuffer::create(&device, converter.layout())?;
    let mut second_readback = Nv12ReadbackBuffer::create(&device, second_converter.layout())?;
    if let Some(error) = block_on(device.pop_error_scope()) {
        return Err(format!("GPU pipeline validation failed: {error}").into());
    }

    let empty_view: &[PreparedSpriteInstance] = &[];
    let batch = TemporalSpriteBatch::pack(views, &[empty_view])?;
    let mut submission = renderer.begin_submission(&device)?;
    let encoded = submission.encode_batch(&queue, &batch)?;
    let pending = submission.encode_nv12_readback(&encoded, &converter, &mut readback)?;
    let frames = submission.submit_and_read_nv12(&device, &queue, pending)?;
    let layout = converter.layout();
    if frames.len() != 1 || frames[0].len() != layout.tight_frame_bytes() {
        return Err("GPU smoke readback dimensions are inconsistent".into());
    }
    let y_bytes = usize::try_from(layout.width())?
        .checked_mul(usize::try_from(layout.height())?)
        .ok_or("GPU smoke Y plane size overflow")?;
    if frames[0][..y_bytes].iter().any(|value| *value != 16)
        || frames[0][y_bytes..].iter().any(|value| *value != 128)
    {
        return Err(
            "GPU smoke NV12 black-frame conversion differs from BT.709 limited range".into(),
        );
    }

    let terrain_plan = TerrainDrawPlan {
        terrain: vec![TerrainDrawOp {
            phase: TerrainDrawPhase::Terrain,
            z_index: 0,
            placement: TerrainPlacement {
                origin: [0.0, 0.0],
                size: [64.0, 64.0],
            },
            source: TerrainDrawSource::Solid { color: 0x20_40_60 },
            mask: None,
            alpha: 1.0,
            blend_mode: SpriteBlendMode::Normal,
        }],
        wall_graffiti: Vec::new(),
        lighting: Vec::new(),
        lighting_composite: None,
        effects: Vec::new(),
    };
    let terrain_mask_bindings = TerrainMaskBindings::default();
    let terrain_batch = TemporalTerrainBatch::compile_phase(
        &[(
            &terrain_plan,
            &terrain_mask_bindings,
            BoardTransform {
                zoom: 1.0,
                position: [0.0, 0.0],
                pivot: [0.0, 0.0],
            },
        )],
        TerrainDrawPhase::Terrain,
        &atlas,
        views,
        [64, 64],
    )?
    .ok_or("GPU smoke terrain plan unexpectedly compiled to no draw")?;
    let mut terrain_uploads = TerrainCommandUploads::create(
        &device,
        NonZeroU32::new(2).expect("constant is nonzero"),
        NonZeroU32::new(1).expect("constant is nonzero"),
    )?;
    let mut submission = renderer.begin_submission(&device)?;
    let lease = submission.lease_batch()?;
    submission.encode_terrain_phase(
        &queue,
        &lease,
        &mut terrain_uploads,
        LeasedTerrainPhase {
            pipeline: &terrain,
            bindings: &terrain_bindings,
            batch: &terrain_batch,
            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
        },
    )?;
    let encoded = submission.prepare_leased_batch(&queue, lease, &batch)?;
    let pending = submission.encode_nv12_readback(&encoded, &converter, &mut readback)?;
    let terrain_frames = submission.submit_and_read_nv12(&device, &queue, pending)?;
    let terrain_reference = rgba8_to_nv12_reference(2, 2, &[0x20, 0x40, 0x60, 0xff].repeat(4))?;
    let terrain_center = usize::try_from(32_u32 * layout.width() + 32)?;
    if terrain_frames.len() != 1
        || terrain_frames[0][terrain_center] != terrain_reference[0]
        || terrain_frames[0][..y_bytes]
            .iter()
            .any(|value| *value != terrain_reference[0])
        || terrain_frames[0][y_bytes..]
            .chunks_exact(2)
            .any(|pair| pair != &terrain_reference[4..6])
    {
        return Err("GPU smoke temporal terrain submission is invalid".into());
    }

    let sprite = PreparedSpriteInstance {
        activation_order: 1,
        layer_order: 0,
        blend_mode: SpriteBlendMode::Screen,
        instance: SpriteInstance {
            transform_x: [1.0, 0.0, 32.0, 0.0],
            transform_y: [0.0, 1.0, 32.0, 0.0],
            size_anchor: [16.0, 16.0, 0.5, 0.5],
            uv_rect: [0.0, 0.0, 1.0, 1.0],
            tint_alpha: [1.0; 4],
            atlas_page: 0,
            visible: 1,
            blur: 2.0,
            has_blur_filter: 1,
        },
    };
    let batch = TemporalSpriteBatch::pack(views, &[std::slice::from_ref(&sprite)])?;
    let mut submission = renderer.begin_submission(&device)?;
    let encoded = submission.encode_batch(&queue, &batch)?;
    submission.encode_sprite_layer_to_lighting(
        &compositor,
        &encoded,
        0,
        wgpu::LoadOp::Clear(wgpu::Color::WHITE),
    )?;
    submission.encode_lighting_composite(&compositor, &encoded, wgpu::LoadOp::Load)?;
    let pending = submission.encode_nv12_readback(&encoded, &converter, &mut readback)?;
    let sprite_frames = submission.submit_and_read_nv12(&device, &queue, pending)?;
    if sprite_frames.len() != 1
        || !sprite_frames[0][..y_bytes].iter().any(|value| *value > 16)
        || sprite_frames[0][y_bytes..]
            .iter()
            .any(|value| *value != 128)
    {
        return Err("GPU smoke filtered sprite/compositor output is invalid".into());
    }

    let vector = PreparedVector {
        entity_id: "smoke",
        node_id: "vector",
        layer: None,
        layer_order: 0,
        z_index: 0.0,
        activation_order: 2,
        transform: Affine2 {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            tx: 32.0,
            ty: 32.0,
        },
        mesh: &vector_mesh,
        alpha: 1.0,
        tint: 0x00_ff_00,
        visible: true,
        blend_mode: SpriteBlendMode::Screen,
        blur: Some(2.0),
    };
    let second_vector = PreparedVector {
        activation_order: 4,
        node_id: "second-vector",
        transform: Affine2 {
            tx: 52.5,
            ..vector.transform
        },
        tint: 0xff_ff_00,
        blend_mode: SpriteBlendMode::Multiply,
        blur: None,
        ..vector.clone()
    };
    let vector_batch = TemporalVectorBatch::pack(views, &[&[vector, second_vector]])?;
    let sprite_red = PreparedSpriteInstance {
        activation_order: 1,
        layer_order: 0,
        blend_mode: SpriteBlendMode::Normal,
        instance: SpriteInstance {
            transform_x: [1.0, 0.0, 32.0, 0.0],
            transform_y: [0.0, 1.0, 32.0, 0.0],
            size_anchor: [64.0, 64.0, 0.5, 0.5],
            uv_rect: [0.0, 0.0, 1.0, 1.0],
            tint_alpha: [64.0 / 255.0, 64.0 / 255.0, 64.0 / 255.0, 1.0],
            atlas_page: 0,
            visible: 1,
            blur: 0.0,
            has_blur_filter: 0,
        },
    };
    let mut sprite_blue = sprite_red;
    sprite_blue.activation_order = 3;
    sprite_blue.instance.size_anchor = [16.0, 16.0, 0.5, 0.5];
    sprite_blue.instance.tint_alpha = [0.0, 0.0, 1.0, 1.0];
    let sprite_batch = TemporalSpriteBatch::pack(views, &[&[sprite_red, sprite_blue]])?;
    let scene_batch = TemporalSceneBatch {
        sprites: sprite_batch,
        vectors: vector_batch,
        display_order: vec![
            SceneDisplayEntry {
                activation_order: 1,
                layer_order: 0,
                kind: SceneDrawableKind::Sprite,
            },
            SceneDisplayEntry {
                activation_order: 2,
                layer_order: 0,
                kind: SceneDrawableKind::Vector,
            },
            SceneDisplayEntry {
                activation_order: 4,
                layer_order: 0,
                kind: SceneDrawableKind::Vector,
            },
            SceneDisplayEntry {
                activation_order: 3,
                layer_order: 0,
                kind: SceneDrawableKind::Sprite,
            },
        ],
    };
    let mut submission = renderer.begin_submission(&device)?;
    let mut poison_scene_batch = scene_batch.clone();
    for instance in &mut poison_scene_batch.vectors.instances[..2] {
        instance.transform_x[2] = 12.0;
        instance.transform_y[2] = 12.0;
    }
    let _first_slot =
        submission.encode_scene_batch(&queue, &vector_pipeline, &poison_scene_batch)?;
    let mut scene_terrain_uploads = TerrainCommandUploads::create(
        &device,
        NonZeroU32::new(2).expect("constant is nonzero"),
        NonZeroU32::new(1).expect("constant is nonzero"),
    )?;
    let scene_terrain = TemporalTerrainSceneBatch {
        terrain: Some(terrain_batch.clone()),
        wall_graffiti: None,
        lighting: None,
        lighting_composite: None,
        effects: None,
    };
    let encoded = submission.encode_render_batch(
        &queue,
        &mut scene_terrain_uploads,
        TemporalRenderBatch {
            vector_pipeline: &vector_pipeline,
            terrain_pipeline: &terrain,
            terrain_bindings: &terrain_bindings,
            compositor: &compositor,
            terrain: &scene_terrain,
            scene: &scene_batch,
        },
    )?;
    let pending =
        submission.encode_nv12_readback(&encoded, &second_converter, &mut second_readback)?;
    let vector_frames = submission.submit_and_read_nv12(&device, &queue, pending)?;
    let blue_reference = rgba8_to_nv12_reference(2, 2, &[0, 0, 255, 255].repeat(4))?;
    let yellow_reference = rgba8_to_nv12_reference(2, 2, &[64, 64, 16, 255].repeat(4))?;
    let background_reference = rgba8_to_nv12_reference(2, 2, &[64, 64, 64, 255].repeat(4))?;
    let center_y = usize::try_from(32_u32 * layout.width() + 32)?;
    let second_vector_y = usize::try_from(32_u32 * layout.width() + 52)?;
    let vector_blur_halo_y = usize::try_from(32_u32 * layout.width() + 41)?;
    let antialiased_vector_edge_y = usize::try_from(32_u32 * layout.width() + 44)?;
    if vector_frames.len() != 1
        || !vector_frames[0][..y_bytes].iter().any(|value| *value > 16)
        || vector_frames[0][center_y] != blue_reference[0]
        || vector_frames[0][second_vector_y] != yellow_reference[0]
        || vector_frames[0][vector_blur_halo_y] <= background_reference[0]
        || !(yellow_reference[0]..background_reference[0])
            .contains(&vector_frames[0][antialiased_vector_edge_y])
    {
        return Err(format!(
            "GPU smoke sprite/vector ordering/filtering is invalid: frames={}, center={}, \
             second={}, halo={}, edge={}, yellow={}",
            vector_frames.len(),
            vector_frames.first().map_or(0, |frame| frame[center_y]),
            vector_frames
                .first()
                .map_or(0, |frame| frame[second_vector_y]),
            vector_frames
                .first()
                .map_or(0, |frame| frame[vector_blur_halo_y]),
            vector_frames
                .first()
                .map_or(0, |frame| frame[antialiased_vector_edge_y]),
            yellow_reference[0],
        )
        .into());
    }

    let info = adapter.get_info();
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "adapter": info.name,
            "backend": format!("{:?}", info.backend),
            "deviceType": format!("{:?}", info.device_type),
            "driver": info.driver,
            "driverInfo": info.driver_info,
            "multiviewLayers": views.get(),
            "nv12Frames": frames.len() + terrain_frames.len() + sprite_frames.len() + vector_frames.len(),
            "nv12FrameBytes": frames[0].len(),
            "blackY": 16,
            "blackUv": 128,
            "filteredSpriteRendered": true,
            "terrainRendered": true,
            "terrainSceneOrchestrated": true,
            "vectorRendered": true,
            "vectorBlurRendered": true,
            "vector4SampleSsaaResolved": true,
            "heterogeneousSceneOrdered": true,
            "residentVectorGeometries": vector_pipeline.resident_geometry_count(),
            "residentVectorVertices": vector_pipeline.resident_vertex_count(),
            "vectorRingSlots": vector_pipeline.slot_count(),
        }))?
    );
    Ok(())
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
