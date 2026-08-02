use std::future::Future;
use std::num::NonZeroU32;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread;
use std::time::{Duration, Instant};

use screeps_arena_native_renderer::{
    GpuTextureAtlas, Nv12BatchConverter, PreparedSpriteInstance, SpriteBlendMode, SpriteInstance,
    SpritePipeline, TemporalSpriteBatch, TemporalSpriteRenderer, TextureAtlas, TextureAtlasPage,
};

const VIEWS_PER_BATCH: u32 = 6;
const IN_FLIGHT_BATCHES: u32 = 3;

fn main() {
    if let Err(error) = run() {
        eprintln!("gpu-throughput: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let width = parse_argument(arguments.next(), 2_048, "width")?;
    let height = parse_argument(arguments.next(), width, "height")?;
    let frame_count = parse_argument(arguments.next(), 16_001_u64, "frame count")?;
    let sprite_count = parse_argument(arguments.next(), 500_u32, "sprite count")?;
    if arguments.next().is_some() {
        return Err("usage: gpu-throughput [width [height [frames [sprites]]]]".into());
    }
    if width == 0 || height == 0 || frame_count == 0 || sprite_count == 0 {
        return Err("benchmark dimensions, frames, and sprites must be positive".into());
    }

    let setup_started = Instant::now();
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
        return Err(format!("{} lacks multiview support", adapter.get_info().name).into());
    }
    let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("Screeps Arena temporal throughput device"),
        required_features: SpritePipeline::REQUIRED_FEATURES,
        required_limits: adapter.limits(),
        ..Default::default()
    }))?;
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
    let views = NonZeroU32::new(VIEWS_PER_BATCH).expect("view count is nonzero");
    let slots = NonZeroU32::new(IN_FLIGHT_BATCHES).expect("slot count is nonzero");
    let mut renderer = TemporalSpriteRenderer::create(
        &device,
        &gpu_atlas,
        width,
        height,
        views,
        NonZeroU32::new(sprite_count).expect("sprite count is nonzero"),
        slots,
    )?;
    let converters = (0..renderer.slot_count())
        .map(|slot| Nv12BatchConverter::create(&device, renderer.target(slot)?))
        .collect::<screeps_arena_native_renderer::Result<Vec<_>>>()?;
    let sprites = benchmark_sprites(width, height, sprite_count);
    let setup_seconds = setup_started.elapsed().as_secs_f64();

    let execution_started = Instant::now();
    let mut remaining = frame_count;
    let mut submissions = 0_u64;
    let mut batches = 0_u64;
    let mut last_submission = None;
    while remaining != 0 {
        let mut submission = renderer.begin_submission(&device)?;
        for converter in &converters {
            if remaining == 0 {
                break;
            }
            let active_views = remaining.min(u64::from(VIEWS_PER_BATCH)) as usize;
            let sprite_views = (0..active_views)
                .map(|_| sprites.as_slice())
                .collect::<Vec<_>>();
            let batch = TemporalSpriteBatch::pack(views, &sprite_views)?;
            let encoded = submission.encode_batch(&queue, &batch)?;
            submission.encode_nv12(&encoded, converter)?;
            remaining -= active_views as u64;
            batches += 1;
        }
        last_submission = Some(submission.submit(&queue));
        submissions += 1;
    }
    let cpu_submit_seconds = execution_started.elapsed().as_secs_f64();
    device.poll(wgpu::PollType::Wait {
        submission_index: last_submission,
        timeout: Some(Duration::from_secs(120)),
    })?;
    let gpu_complete_seconds = execution_started.elapsed().as_secs_f64();
    let info = adapter.get_info();
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "adapter": info.name,
            "driver": info.driver,
            "driverInfo": info.driver_info,
            "width": width,
            "height": height,
            "frames": frame_count,
            "spritesPerFrame": sprite_count,
            "viewsPerBatch": VIEWS_PER_BATCH,
            "inFlightBatches": IN_FLIGHT_BATCHES,
            "batches": batches,
            "submissions": submissions,
            "setupSeconds": setup_seconds,
            "cpuSubmitSeconds": cpu_submit_seconds,
            "gpuCompleteSeconds": gpu_complete_seconds,
            "framesPerSecond": frame_count as f64 / gpu_complete_seconds,
            "rgbaGpixelsPerSecond": frame_count as f64 * f64::from(width) * f64::from(height)
                / gpu_complete_seconds / 1e9,
            "includesNv12Conversion": true,
            "includesReadback": false,
            "includesVideoEncode": false,
        }))?,
    );
    Ok(())
}

fn benchmark_sprites(width: u32, height: u32, count: u32) -> Vec<PreparedSpriteInstance> {
    let columns = (f64::from(count).sqrt().ceil() as u32).max(1);
    let rows = count.div_ceil(columns);
    let cell_width = width as f32 / columns as f32;
    let cell_height = height as f32 / rows as f32;
    (0..count)
        .map(|index| {
            let column = index % columns;
            let row = index / columns;
            PreparedSpriteInstance {
                activation_order: index,
                layer_order: 0,
                blend_mode: SpriteBlendMode::Normal,
                instance: SpriteInstance {
                    transform_x: [1.0, 0.0, (column as f32 + 0.5) * cell_width, 0.0],
                    transform_y: [0.0, 1.0, (row as f32 + 0.5) * cell_height, 0.0],
                    size_anchor: [cell_width * 0.75, cell_height * 0.75, 0.5, 0.5],
                    uv_rect: [0.0, 0.0, 1.0, 1.0],
                    tint_alpha: [1.0, 1.0, 1.0, 1.0],
                    atlas_page: 0,
                    visible: 1,
                    blur: 0.0,
                    has_blur_filter: 0,
                },
            }
        })
        .collect()
}

fn parse_argument<T>(value: Option<String>, default: T, name: &str) -> Result<T, String>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value.map_or(Ok(default), |value| {
        value
            .parse()
            .map_err(|error| format!("invalid {name} {value:?}: {error}"))
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
