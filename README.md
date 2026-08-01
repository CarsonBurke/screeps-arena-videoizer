# Screeps Arena Videoizer

Deterministic, board-only video generation for Screeps Arena replays.

This does not screen-record the game in real time. The patched Pixi renderer is
driven by an exact virtual clock, applies replay states programmatically, renders
the requested frames, and streams H.264 to a host `ffmpeg` process for MP4
remuxing. It never waits for real-time playback; actual throughput is determined
by renderer state application and the available WebCodecs encoder.

## What is deterministic

- Renderer tick and frame times use exact rational arithmetic, including fractional rates.
- Tick 1 is installed at virtual time zero after the initial tick-0 frame, matching
  the client transition semantics.
- Pixi's application and shared animation tickers run from virtual time.
- The game action manager advances in fixed 60 Hz substeps by default.
- The normal RAF loop, implicit Pixi render callback, and game animate callback
  are detached, preventing wall-clock drift and double updates.
- Renderer randomness is seeded in the replay component constructor, before the
  child renderer instantiates metadata actions. After taking clock control,
  capture synchronously tears down any replay objects and finite effects the UI
  rendered while loading, rebuilds persistent decoration actions, and resets
  existing AnimatedSprite instances to frame zero before applying tick 0.
- Every expected tick and frame is checked before completion. The launcher also
  verifies the decoded MP4 frame count.
- Owner-only Canvas2D visuals are uploaded as a Pixi texture and composited into
  the captured WebGL canvas when they are available.
- The camera is computed from the renderer's world dimensions and reasserted
  before every frame, so the entire board remains centered inside fixed pixel
  padding even if a client observer tries to move it.

## Requirements

- Linux with a GPU-backed X display
- Steam or Flatpak Steam with Screeps Arena installed
- `ffmpeg` and `ffprobe`
- Node.js (for patching and tests)
- `Xvfb` only for the optional slow `--headless` debugging mode

## Setup

```bash
npm run check
./patch-screeps-arena.js
```

The patcher auto-detects native and Flatpak Steam installations. Override it with
`SCA_APP_ROOT=/path/to/ScreepsArena`. It keeps `*.videoizer.bak` files and copies
the testable runtime modules into the installed app. Re-running the patcher is
safe and upgrades an earlier videoizer patch.

## Generate a video

```bash
./record-board-x11.sh "https://arena.screeps.com/game/A1CWF344IR" \
  out/A1CWF344IR.mp4
```

The defaults are 2048×2048, 30 fps, 8 frames/tick (3.75 ticks/s), a 60 Hz
simulation clock, full-map auto-fit with 32 pixels of padding, and 24 Mbps H.264:

```bash
WIDTH=2048 HEIGHT=2048 FPS=30 \
TICKS_PER_SECOND=3.75 SIMULATION_FPS=60 \
BITRATE=24000000 \
./record-board-x11.sh <replay> out/video.mp4
```

`TICKS_PER_SECOND` is the clearest way to choose apparent playback speed. For
compatibility, omitting it uses `FPS / FRAMES_PER_TICK`; the default
`FRAMES_PER_TICK=8` is therefore also 3.75 ticks/s at 30 fps.

Useful framing and operational settings:

```bash
BOARD_ZOOM=auto BOARD_PADDING=32 BOARD_PAN_X=0 BOARD_PAN_Y=0
PRELOAD_CONCURRENCY=4 COMPILER_UNIT_TICKS=50 REPLAY_IR=1
CAPTURE_TIMEOUT=1800
RENDER_DISPLAY=:0
EXTRA_FLAGS="..."
```

`BOARD_ZOOM=auto` is the safe default: it contains the complete 100×100 map at
any square, landscape, or portrait output size. A numeric zoom is an explicit
manual override and may crop the map. Pan values are pixel offsets applied after
centering. The terrain intermediate texture is automatically raised to the
projected board dimensions, up to 4096²; set
`SCREEPS_ARENA_BOARD_CAPTURE_TEXTURE_SIZE` only when deliberately overriding it.

Replay network chunks are fetched with bounded concurrency and states are mapped
once. `COMPILER_UNIT_TICKS` defines exact 50-tick ReplayIR planning/ownership
boundaries; it does not make Pixi faster. The present compatibility renderer
still consumes states and frames sequentially because its live action graph is
stateful, and telemetry reports that explicitly. The native temporal backend in
[PERFORMANCE.md](PERFORMANCE.md) uses portable 2–6-view wgpu multiview passes,
with larger frame units split across those bounded batches.

`REPLAY_IR=1` additionally retains a lossless, content-addressed ReplayIR and
the exact renderer contract beside the run telemetry. The IR stores entity
lifetimes, columnar property segments, object ordering, global state, owner
visual commands, the renderer random seed, its exact post-scene-construction
PRNG state, an indexed action-log stream, and a compact columnar renderer graph
of object, processor, action, and preprocessor lifecycle events;
every source tick can be reconstructed exactly. Session-local badge blobs and
remote decoration textures are embedded as deterministic data URLs. The
renderer contract fingerprints the official metadata calculations, predicates,
processors, action trees, drawing methods, expression operators, resources,
layers, world settings, arena decorations, and the decoded static wall/swamp
grid. Generated runtime IDs are normalized, the exact bundled client JavaScript
is hashed, and loaded artifacts are structurally checked, rehashed, and frozen
before use.
The IR also records a custom state-transition duration separately from apparent
playback speed. An accelerated backend therefore fails closed when it encounters
a visual semantic or renderer implementation it has not implemented, or a
modified artifact.

The source tree includes `native-renderer`, a Rust frontend for this boundary.
It accepts only canonical ReplayIR v7/renderer-contract v5 artifacts, checks
both fingerprints and all structural invariants, and plans independently
addressable absolute-time frame batches:

```bash
cargo run --release --manifest-path native-renderer/Cargo.toml -- \
  /path/to/capture.replay-ir.json 256
```

Run the headless hardware smoke path independently of Steam with:

```bash
cargo run --release --manifest-path native-renderer/Cargo.toml \
  --bin gpu-smoke
```

It creates the multiview sprite/blur, vector, terrain, lighting-compositor, and
NV12 pipelines, submits real Vulkan batches, and reads back four converted
frames. It verifies the BT.709-limited black bytes, a temporal terrain draw, and
non-black output from a filtered, screen-blended sprite. A shared sprite →
vector → sprite scene checks overlapping display order, whole-node vector blur,
and a partially covered subpixel edge after the portable four-sample
supersampling resolve. The vector check uses two instance indices and the second
in-flight ring slot while confirming that two registrations of the same mesh
occupy one resident geometry entry.
`VK_ICD_FILENAMES` can select a particular installed ICD when diagnosing a
multi-GPU machine.

The command accepts a file path or `-` for a streamed artifact. It reports the
validated workload, compiles the 17 retained processor kinds and all 14 nested
action kinds into typed plans, verifies object/action/processor lifecycle
intervals against entity lifetimes, computes conservative peak output budgets,
resolves constructor-time object, processor, and nested-action expressions in
the captured global event/PRNG order, and prepares a deterministic premultiplied
texture atlas. The atlas preserves both supersampled raster extents and Pixi's
intrinsic texture dimensions, includes materialized arena-decoration PNG,
JPEG, WebP, and SVG assets at their official dimensions, and is cached by
renderer-contract fingerprint and raster settings under
`~/.cache/screeps-arena-videoizer/atlas` (override with
`SCREEPS_ARENA_ATLAS_CACHE_DIR`). Per-key OS locks and a post-lock recheck
ensure one cold worker performs the decode and pack while parallel replay
workers reuse its atomically published result. The native crate reconstructs the official
smoothed wall, swamp, and per-user private-rampart paths from the authenticated
static grid and dynamic entity tracks and assigns each distinct geometry a
stable cache fingerprint. It evaluates geometry only at exact terrain-relevant
track/lifetime change points and caches separate straight-alpha fill/stroke masks by
actual path component, captured terrain raster size, and format version under
`~/.cache/screeps-arena-videoizer/terrain` (override with
`SCREEPS_ARENA_TERRAIN_CACHE_DIR`). Per-key OS file locks prevent cold parallel
workers from rasterizing the same path simultaneously and release
automatically if a worker exits. A bounded `Arc`-backed LRU shares repeated
components in-process. Exact geometry spans distinguish long-lived terrain,
which is published for cross-replay reuse, from one-tick dynamic misses, which
stream without accumulating in RAM or creating disk-cache files. Styled strokes,
using the official defaults or first matching landscape-decoration widths, are
converted into linear fill and visible-stroke paint contributions so bilinear
filtering and mip reduction do not thicken antialiased edges. A typed draw contract now compiles the exact floor,
ground mask, four exits, swamp base and animated masked noise, wall paint with
precomposed noise, landscape foreground, private ramparts, wall shadow, and
lighting-layer order and blends. Swamp and rampart paint state follows the
retained renderer's path-rebuild latching, and animation phase uses the exact
replay apply-tick clock. The draw contract and masks are still not finished
output pixels, but the native GPU path now keeps coverage masks resident with
full mip chains, builds atlas-safe color mips with wrapped repeat gutters,
compiles ordered multiview terrain phases, bakes wall base/noise into
fixed-size mipless RGBA8 layers with the retained intermediate quantization,
and precomputes the retained five-tap quality-four wall blur in eight
quantized passes. Mask, wall, and blur banks reject mismatched extents and
out-of-range layer references; each defaults to a conservative 256 MiB limit
so callers must partition unusually diverse replays into geometry windows
instead of silently allocating multi-gigabyte texture arrays. Empty banks use
1×1 physical placeholders. Terrain command buffers use disjoint one-shot
upload ranges and ordered buffer copies, so several recorded phases cannot
alias the final queued instance data. Full replay device orchestration,
wall-graffiti decorations, and representative pixel-golden validation remain.
The native crate also contains a bounded
wgpu multiview sprite path that renders two to six independent timestamps into
a texture array in one pass from Pixi-style top-left pixel coordinates. It
preserves Pixi's RGBA8 UNORM color arithmetic and supports ordered normal,
additive, multiply, and screen blend runs. Draw runs retain their metadata-layer
identity even when adjacent layers share a blend mode. A leased target can
receive terrain before per-layer sprite runs, and a per-ring-slot lighting
texture array supplies the retained multiply-filter intermediate without
serializing temporal views. The first draw-node adapters lower root containers,
the retained generic `container`/`sprite` processors, supersampled
premultiplied `circle` graphics, and state-derived `resourceCircle` energy/power
fills. The `userBadge` adapter samples the authenticated per-tick global user
map, packs self-contained badge images into the content-addressed atlas, and
uses the retained circle fallback when no badge is available. Image badges keep
the renderer's temporary-scope lifetime, including repeated children that
remain until object teardown. These adapters include
intrinsic sizing, parent transforms, anchors, pivots, tint, visibility, and
blend mode. GPU instances carry a full 2D affine transform so nested nonuniform
transforms retain shear. Sprites with Pixi `BlurFilter` are isolated without
changing display order, rendered into a shared bounded pair of temporal scratch arrays,
processed through the retained quality-four four-horizontal/four-vertical
five-tap passes using independently animated strengths in every view. The
sprite's blend mode is applied when its filter input is rendered and the last
filter pass blends directly into the scene with Pixi's normal filter state,
avoiding an extra RGBA8 quantization. Main targets plus blur scratch are capped
at 512 MiB. Tessellated draw and site-progress vectors share the same temporal
target and heterogeneous activation order as sprites. Their immutable
content-addressed meshes are deduplicated in one resident vertex bank, while
preallocated ring slots receive only animated instance values per microbatch.
Each vector node is rasterized into a shared 2×-per-axis multiview scratch array
and box-resolved to four-sample antialiased RGBA8 before composition. Filtered
vectors then reuse the sprite renderer's bounded ping/pong arrays for the same
quality-four, eight-pass blur. The node blend applies to the filter input and
the last pass uses normal Pixi filter blending; an unfiltered node applies its
blend during the resolved scene composite.
A typed native runtime now instantiates all retained
actions and preserves their fixed-step update, finish, reset, nesting, repeat,
spawn, and easing behavior. Its global action-manager equivalent preserves
handle insertion order, finish versus cancellation, and binding to detached
node activations when a processor replaces a public scope ID. An authenticated
event-driven scene runtime now applies generic object/processor/action creation,
replacement, destruction, cross-scope node deletion, late target lookup, and
root-alpha changes. It maintains a dense visible-activation set, so preparing a
frame visits current nodes instead of scanning every historical processor
activation; per-tick renderer events are also read directly from their columnar
range without allocating a temporary vector. Metadata layer declaration order,
lifted-node `zIndex` sorting, stable traversal order, and ordinary container
child insertion order are compiled at lifecycle changes. Action-mutated
position, scale, rotation, alpha, tint, and blur values feed reusable affine
GPU-instance buffers, and the exact apply-tick/advance/render stream is packed
into bounded multiview microbatches. The lifecycle path also resolves
`runAction` against existing scope/root targets without evaluating unreachable
actions or shifting the renderer PRNG. Root-container identity is tagged
separately from ordinary JavaScript scope keys, and property-key coercion
matches falsey, numeric, array, object, and encoded BigInt values. The retained
half-transition `disappear` fade pins its exact object generation until cleanup,
so an overlapping reuse of the entity ID cannot lose the old fade or delete the
new subtree.
Distinct in-flight batches use leased
buffer/target slots and must be submitted through one owning command context,
preventing later uploads from changing earlier recorded passes. A leased
terrain-scene submission now records terrain and wall foreground phases, the
heterogeneous sprite/vector scene, the per-slot lighting intermediate and
multiply composite, and final terrain effects into one ring target before NV12
conversion. Remaining dedicated processor lowering, the replay output driver
and visual overlays,
object-local Pixi filter-frame edge parity,
pixel-golden validation, final compositor wiring, and GPU-resident
hardware-encoder interop remain under development, so the command does not
produce a video yet.
The native library now includes a multiview RGBA8-to-BT.709 NV12 conversion
pass, bounded aligned readback, and a long-lived FFmpeg H.264 sink with exact
frame-size validation, color metadata, bounded pipe writes, failure capture,
and atomic publication.
That readback path is a validation/fallback oracle; it is not presented as the
final throughput path because host transfer alone exceeds the target budget.

The backend-independent JavaScript semantic core currently implements all 14 action types
and all five expression operators inventoried from the retained Arena renderer.
It also runs the official calculation functions with their exact retained-value,
dependency, path, condition, and lifetime semantics, streaming the results into
columnar ReplayIR tracks without retaining another full replay. The processor
compiler now follows the official GameObject creation order and emits exact
run/destruct and action run/finish boundaries without duplicating metadata
trees. State-, calculation-, and target-relative action values remain
late-bound, and `$random` values are regenerated from the saved PRNG checkpoint
in event order. The native frontend now compiles those action trees and
lifecycle intervals, including the retained renderer's deliberately shared
processor scope IDs, samples state/calculation roots at every activation, and
resolves nested action parameters exactly once in the captured PRNG order. It
instantiates the resulting action trees with native equivalents of the official
mutation semantics, drives the generic container/sprite/vector scene through the
authenticated lifecycle stream, and streams the resulting GPU instances into
temporal batches without retaining replay-wide frame state. The current action
runtime is still sequential rather than an independently shardable stateless
track. Additional processor-specific native render-node adapters, unified
terrain/scene orchestration, and representative filter-frame pixel goldens
remain compatibility work; until every active adapter is golden-tested, the accelerated renderer
must reject those contracts rather than emit a subtly different video.

Processor payloads are pure in the retained contract. A future contract that
places `$random` in object data, object texture, or a processor payload is
rejected until that processor's exact lazy field-evaluation order is
implemented; randomized action parameters already use the exact typed order.

Use `--headless` only for debugging. Xvfb has no hardware WebGL on a normal
setup and is much slower.

## Outputs and diagnostics

The MP4 is written to the requested path. Per-run debug, ffmpeg, and JSON
telemetry files are retained in:

- Native Steam: `~/.cache/screeps-arena-videoizer/`
- Flatpak Steam (host path):
  `~/.var/app/com.valvesoftware.Steam/cache/screeps-arena-videoizer/`

The launcher muxes into a same-directory temporary MP4 and publishes it only
after the renderer completion marker, successful telemetry, and decoded frame
count all agree. A crash cannot silently replace the requested output with a
truncated file.

Telemetry separates state fetching/application, action/ticker updates, Pixi
rendering, `VideoFrame` creation, encoder backpressure, and flush time. The
capture configuration is also embedded in the replay URL so a pre-existing Steam
process cannot reuse stale environment settings.

The current raw-H.264 remux is constant-frame-rate. If a replay endpoint falls
between frame-grid points, the renderer still samples that exact endpoint, but
the MP4 holds the last sample for one normal frame interval (less than `1/FPS`
of padding). A timestamp-aware native muxer is part of the next backend.

## Performance reality

The current Linux Electron build reports only a software H.264 WebCodecs encoder
on the tested NVIDIA system. Pixi rendering is GPU accelerated, but NVENC is not
being used. A measured 286-tick, 1280², one-frame/tick run generated and validated
287 frames in 14.6 s after bypassing the invisible Angular UI state pipeline;
6.1 s was renderer state application and 6.8 s was encoder flush.

That is a correct fast-offline path, but it cannot turn a 2,000-tick replay at
30 fps and 2–5 ticks/s (12,001–30,001 frames) into a video in two seconds. That
target requires 6,000–15,000 complete frames/s and temporal GPU batching, not
just removal of real-time waits. See [PERFORMANCE.md](PERFORMANCE.md) for the
measured bottlenecks and the GPU-native architecture needed for that tier. The
current runtime is the compatibility reference and usable fallback for that
backend; representative pixel-golden validation is still required before
calling a future renderer visually equivalent.
