//! Validates the Vulkan external-memory -> CUDA array -> AV1 NVENC bridge.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread;

use screeps_arena_native_renderer::{
    Rational, SpritePipeline, VulkanExternalNv12, VulkanNvencConfig, VulkanNvencEncoder,
};

const WIDTH: u32 = 2_048;
const HEIGHT: u32 = 2_048;
const RING_SIZE: usize = 16;
const FRAMES: u64 = RING_SIZE as u64 + 1;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..Default::default()
    });
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
    }))?;
    let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("Vulkan NVENC smoke device"),
        required_features: SpritePipeline::REQUIRED_FEATURES,
        required_limits: adapter.limits(),
        ..Default::default()
    }))?;
    let targets = (0..RING_SIZE)
        .map(|_| VulkanExternalNv12::create(&device, WIDTH, HEIGHT).map(Arc::new))
        .collect::<Result<Vec<_>, _>>()?;

    let encoder =
        VulkanNvencEncoder::new(VulkanNvencConfig::new(WIDTH, HEIGHT, Rational::new(30, 1)?))?;
    let mut ring = encoder.create_ring(targets.clone())?;
    let mut initial_render = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("initial NVENC smoke targets"),
    });
    for target in &targets {
        initial_render.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("initial NVENC smoke target"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target.view(),
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.25,
                        g: 0.0,
                        b: 0.0,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
    }
    let initial_submission = queue.submit(Some(initial_render.finish()));
    VulkanExternalNv12::wait_for_submission(&device, initial_submission)?;
    let ring_capacity = ring.capacity();
    let mut frames = Vec::with_capacity(FRAMES as usize);
    let first_slots = (0..RING_SIZE)
        .map(|_| ring.acquire_slot().ok_or("NVENC ring has no free slot"))
        .collect::<Result<Vec<_>, _>>()?;
    for slot in first_slots {
        ring.submit(slot)?;
    }
    frames.push(
        ring.drain_oldest()?
            .ok_or("full NVENC ring had no drainable frame")?,
    );
    let reused_slot = ring.acquire_slot().ok_or("NVENC ring has no free slot")?;
    let mut rewrite = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("rewritten NVENC reuse target"),
    });
    {
        rewrite.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("rewritten NVENC reuse target"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: targets[reused_slot].view(),
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::WHITE),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
    }
    let rewrite_submission = queue.submit(Some(rewrite.finish()));
    VulkanExternalNv12::wait_for_submission(&device, rewrite_submission)?;
    ring.submit(reused_slot)?;
    frames.extend(ring.finish()?);
    if frames.len() != FRAMES as usize
        || frames
            .iter()
            .enumerate()
            .any(|(index, frame)| frame.frame_index != index as u64 || frame.data.is_empty())
    {
        return Err("direct Vulkan NVENC smoke returned an invalid packet".into());
    }
    let stream = frames
        .iter()
        .flat_map(|frame| frame.data.iter().copied())
        .collect::<Vec<_>>();
    std::fs::write("/tmp/vulkan-nvenc-smoke.av1", stream)?;
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "adapter": adapter.get_info().name,
            "width": WIDTH,
            "height": HEIGHT,
            "frames": frames.len(),
            "bytes": frames.iter().map(|frame| frame.data.len()).sum::<usize>(),
            "ringCapacity": ring_capacity,
            "output": "/tmp/vulkan-nvenc-smoke.av1",
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
