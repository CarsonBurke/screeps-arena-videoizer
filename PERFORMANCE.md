# Performance architecture

## Measured baseline

Hardware: NVIDIA RTX 5090, loaded driver 610.43.02 at benchmark time. Client:
Screeps Arena 1.0.13, Electron 39, PixiJS 7.4.3.

For a public 286-tick replay at 1280×1280, 30 fps, one frame/tick:

| Pipeline | Frames | Total | Generated fps | State apply | Pixi render | Encoder wait/flush |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Original injected loop | 287 | 12.4 s encoder completion | 23.1 | not separated | not separated | NVENC 0% |
| Exact virtual runtime through Angular wrapper | 287 | 23.5 s | 12.2 | 18.1 s | 1.76 s | 1.42 s |
| Exact virtual runtime direct to renderer core | 287 | 14.6 s | 19.7 | 6.1 s | 0.55 s | 7.25 s |

The direct path removes UI-observable publication and control timers that never
affect a board-only frame. The output was independently decoded and contained all
287 expected frames. NVIDIA encoder utilization remained 0%: WebCodecs selected
`hardwareAcceleration: "no-preference"` after the hardware-preferred H.264
configuration was unsupported.

The full-speed target must account for output cardinality:

| Apparent rate | 2,000 ticks at 30 fps | Required for 2 s |
| --- | ---: | ---: |
| 5 ticks/s | 12,001 frames | ~6,000 frames/s |
| 3.75 ticks/s | 16,001 frames | ~8,000 frames/s |
| 2 ticks/s | 30,001 frames | ~15,000 frames/s |

No sequential Pixi/WebCodecs loop can reach those rates. Multiple animation
substeps per output frame increase simulation work further.

## Tier 1: implemented deterministic offline renderer

The repository currently implements the correctness foundation needed by every
faster backend:

1. Fetch all 100-tick replay and owner-visual chunks with bounded network
   concurrency, then map every state once.
2. Apply each transition target directly to the renderer core.
3. Advance actions and both relevant Pixi tickers on one exact virtual timeline.
4. Render precisely one explicit frame for each output timestamp.
5. Apply event-driven encoder and FIFO backpressure.
6. Assert tick, frame, timestamp, encoded-chunk, and byte-count invariants.
7. Persist enough telemetry to distinguish state, render, and encode costs.

Before tick 0, the compatibility renderer now synchronously destroys any
GameObjects and finite effect containers produced while the replay page was
loading, clears the live action manager, rebuilds persistent decoration actions,
and resets persistent AnimatedSprites. This reset preserves the seeded PRNG
checkpoint, so capture output no longer depends on which tick happened to be
visible when initialization completed.

Raw Annex-B output intentionally uses WebCodecs `latencyMode: "realtime"`.
Quality mode may emit reordered B-frames, while a raw elementary stream discards
the chunk timestamps; testing that combination produced missing frames and
non-monotonic timestamps after remuxing.

The implemented 50-tick state units are replay-data/compiler boundaries, not
resumable Pixi scenes. The live Pixi action manager retains prior calculations,
one-shot scopes, and unbounded repeat actions, so a preceding raw state plus a
finite preroll cannot reproduce every boundary exactly.

### Lossless ReplayIR boundary

`REPLAY_IR=1` now emits a versioned, content-addressed artifact containing:

- compact columnar entity lifetimes and property segments, including the
  renderer's explicit-`undefined` fields;
- exact per-tick object ordering and global replay state;
- an indexed action-event stream whose payloads remain in the lossless property
  tracks;
- owner Canvas2D visual commands, embedded user-badge assets, the replay-derived
  random seed and exact post-scene-construction PRNG state, the exact fixed
  substep rate, and the independent renderer state transition duration;
- the official renderer metadata calculations, processor graph, nested action
  trees, resource map, immutable world configuration, arena decorations,
  decoded static wall/swamp grid, and a contract fingerprint;
- exact columnar calculation-output tracks produced with the official calculation
  functions, including retained/skipped values, previous-state dependencies,
  per-lifetime reset, calculation-on-calculation dependencies, paths, payloads,
  and `when` conditions;
- an O(1)-per-tick columnar index of object creation/removal, processor
  run/destruct, action run/finish, preprocessor, and root-alpha lifecycle
  events. Entity IDs and metadata IDs are interned, while the canonical
  renderer contract remains the single copy of payload/action trees;
- a complete inventory of object, processor, action, preprocessor, calculation,
  predicate, expression, drawing, and layer semantics that a backend must
  explicitly support.

Session-local blob URLs and remote arena-decoration textures are materialized
into data URLs; renderer-generated random IDs are removed from artifact
identity. The client bundle is hashed to pin closed-over calculations
and processor implementations that cannot be recovered from
`Function#toString`. Both the replay and renderer contract are deeply immutable
in-process; loaded artifacts are rehashed once and then deeply frozen. The
support check fails closed on any missing semantic. Reconstruction tests cover
spawn, update, explicit undefined, property removal,
disappearance/reappearance, action logs, visual overlays, and object ordering.

Measured compilation results:

| Input | Entities | Compile + stringify | Artifact |
| --- | ---: | ---: | ---: |
| Retained real 543-tick arena states | 442 distinct | 146.3 ms | 2.86 MiB including contract |
| Synthetic 2,000 ticks × 100 always-live moving objects | 100 distinct | 302 ms | 7.7 MiB |
| Synthetic 2,000 ticks × 300 always-live moving objects | 300 distinct | 967 ms | 23.0 MiB |
| Synthetic 2,000 ticks × 500 always-live moving objects | 500 distinct | 1.70 s | 38.4 MiB |

The synthetic cases deliberately change position, hits, tick state, and periodic
action logs across every object. Peak RSS growth was approximately 137, 262, and
371 MiB respectively. Strict nested-value validation is included. After a
37.1 ms one-time validation and deep freeze of a serialized 2,000-tick,
100-entity artifact, cached reconstruction sustained approximately 13,900
ticks/s. The 500-object compiler case alone consumes the whole latency budget
and has high peak memory, so a bounded-memory binary/native compiler is still
required at those densities.
These measurements establish the compiler boundary cost; they are not
end-to-end video benchmarks.

Calculation outputs are streamed directly into tracks rather than retained as a
second tick dataset. In a deliberately high-churn four-calculation benchmark,
2,000 ticks × 100 objects compiled and serialized in 1.10 s (16.1 MiB,
224 MiB RSS growth); 300 objects took 3.42 s (48.2 MiB, 602 MiB RSS growth).
That worst-case duplication is not fast enough for the final target. The native
compiler must either fuse these tracks into its bounded-memory pass or lower
supported formulas to temporal GPU expressions, retaining output tracks only
for closure-dependent calculations. Random calculations currently fail closed:
their RNG calls are interleaved with processor/action construction in the
official renderer and cannot be separated without changing animation identity.

A separate full-churn processor-graph stress case (2,000 ticks × 100 objects,
one changing calculation and three scheduled processors per object per tick)
compiled 602,401 lifecycle events and serialized them in 1.93 s. The columnar
artifact was 15.2 MiB with about 293 MiB RSS growth. The earlier naive event
representation took 6.74 s, occupied 134.5 MiB, and grew RSS by roughly
1.1 GiB; interning metadata/entity IDs and retaining contract trees only once
removed that duplication. This is still JavaScript/compiler performance, not
video rendering throughput.

The native renderer frontend now strictly loads canonical ReplayIR v8 artifacts,
verifies both SHA-256 fingerprints, validates every track/lifetime/event index,
recomputes the metadata semantic inventory, and builds an entity index. It
structurally compiles all 17 retained processor kinds and 14 nested action kinds,
separates unique event-definition IDs from deliberately shared renderer scope
IDs, and converts object, processor, and action lifecycle events into exact
half-open tick intervals. The interval pass verifies object lifetimes and
computes conservative peak fixed-output and dynamic-processor budgets without
running JavaScript per frame. A subsequent typed pass samples replay
state/calculation tracks at each activation, resolves object data, processor
payloads, and recursive action parameters exactly once, and advances the saved
Mulberry32 state in the renderer's global constructor order. Its absolute-time
planner reproduces the reference timeline with exact rational frame counts,
endpoint timestamps, shortened off-grid intervals, tick selection, and
state-transition progress. A sequential event iterator additionally reproduces
the compatibility runtime's apply-tick/advance/render order and splits action
updates at both output-frame and global fixed-substep boundaries. The typed
native action runtime implements all 14 retained actions, including official
finish/reset quirks, nested sequence/spawn/repeat behavior, easing deltas,
shortest-path rotation, per-step tint flooring, and filter-property updates.
The native global action manager keeps handle insertion order and distinguishes
finish from cancellation while binding handles to unique node activations, so a
replacement with the same public scope ID cannot receive mutations intended for
the detached Pixi object. The authenticated lifecycle stream now drives generic
object, processor, action, cross-scope node-deletion, late-addressability, and
root-alpha mutations into that manager. Frame preparation walks its dense set of
currently visible activations rather than all historical templates, and event
lookup borrows the exact columnar tick range without a temporary allocation.
Mutated position, scale, rotation, alpha, tint, and blur values feed the affine
sprite-preparation boundary. Metadata layer order, lifted-node `zIndex`, stable
traversal order, and ordinary child insertion order are rebuilt only at
lifecycle changes. The runtime streams exact frames into bounded multiview
microbatches; when visibility changes inside one batch, a deterministic
topological merge preserves every active view's draw sequence. Each frame
sample remains independently addressable, but the action evaluator is still
sequential rather than a stateless, out-of-order track.

A release-build synthetic workload containing 2,000 ticks, 500 always-live
sprites, 500 continuously repeating rotations, 16,001 output frames, and a
2048² render configuration compiled and packed those frames in a 2.01 s median
across three runs (2.00–2.08 s) using portable six-view batches. Median artifact
loading was another 206 ms and the atlas was a cache hit. This measures CPU
scene evaluation and batch packing only: it does not include GPU rasterization,
pixel validation, NV12 conversion, video encoding, or muxing, and therefore is
not an end-to-end throughput claim.

Renderer resources are now embedded into new ReplayIR captures, decoded
fail-closed as SVG, PNG, JPEG, or WebP, premultiplied, packed deterministically with extruded
edges, and normalized into equal-size texture-array pages. On the retained
106-resource renderer contract, rasterization and packing took about 1.08 s in
a debug build before the equal-layer normalization change. A versioned binary
atlas cache is keyed by the authenticated renderer-contract fingerprint and
raster settings, stores both raster and intrinsic Pixi extents, checksums its
complete binary payload, validates all counts and bounds, recovers atomically
from corruption, and removes that image decode and packing cost from subsequent
replays using the same contract. Materialized landscape assets retain their
official dimensions (currently up to 2049 px in the retained arena set)
without relaxing the ordinary 1024 px sprite limit. A per-key OS lock and
post-lock recheck prevent parallel cold workers from all decoding and
allocating the same large atlas.

New renderer-contract v5 captures also retain the arena decorations and the
decoded static terrain cells that the official client otherwise keeps only in
its live stage. The native terrain compiler combines those cells with dynamic
constructed walls, swamps, and private ramparts at a requested tick, reproduces
the official smoothed SVG path algorithm (including diagonal rampart
connections), and computes a stable geometry fingerprint. This establishes the
content-addressed input for a cross-replay full-map cache. Exact change-point
analysis avoids regenerating unchanged paths at every replay tick. Separate
straight-alpha wall, swamp, and per-user rampart fill and stroke masks are rasterized at
the renderer's captured `RENDER_SIZE`/`size`. Each actual path component is
cached independently, so a rampart change does not duplicate unchanged
full-screen wall/swamp masks. Per-key OS-released advisory locks coordinate
cold parallel workers; entries use atomic publication, checksum validation,
and corruption recovery. A bounded `Arc`-backed LRU reuses repeated components
without rereading or copying them. Exact half-open geometry spans keep
long-lived terrain durable across replays, while one-tick misses stream without
synchronous cache writes or lock-file growth. Reusable resident bytes are
tracked incrementally. A synthetic static 100×100 arena over
2,000 ticks produced one geometry: at the earlier fill-only milestone,
change-point geometry compilation took 0.135 ms and a warm 512² two-mask cache
load took 0.54 ms (source-only CLI measurement, not a current styled-mask
benchmark). Stroke widths now follow the official defaults and first matching
landscape decoration. A typed native draw contract now orders the official
floor, multiplicative ground mask, exits, swamp base and masked noise, wall
paint and precomposed noise, landscape foreground, ramparts, wall shadow, and
lighting intermediate and composite. It preserves the retained processor's
path-rebuild latching for swamp texture mode and private-rampart colors.
Persistent swamp tiling offsets are reset when capture acquires the stopped
clock, and later geometry rebuilds derive phase from exact apply-tick time
(tick one is installed at time zero). Fill and visible-stroke contributions
remain linear through bilinear and mip filtering. The native GPU implementation
now uploads deduplicated coverage with full mip chains, builds atlas-safe
wrapped color mips in one pass, compiles 2–8-view ordered terrain draws, bakes
wall paint/noise into fixed-size mipless RGBA8 layers, and precomputes the exact
four-horizontal plus four-vertical five-tap blur with RGBA8 quantization and
the filter pool's asymmetric power-of-two edge sampling. The resident banks
fail closed on incompatible extents or layer indices, use 1×1 placeholders
when empty, and enforce a 256 MiB default per-bank ceiling. Replay integration
must split inputs that exceed that ceiling into temporal geometry windows
rather than keeping every full-resolution geometry resident at once. A
one-shot upload arena assigns every pass a disjoint queue-write range and
records an ordered copy immediately before the draw; multiple phases in one
command buffer therefore cannot all observe the final host upload. These paths
are statically validated and unit-tested. Live Vulkan validation now creates
every pipeline and submits a temporal terrain phase into the shared scene
target before NV12 readback. The leased submission boundary can now record a
complete compiled terrain-scene microbatch in phase order—terrain and wall
foreground, heterogeneous objects, lighting intermediate/composite, then
effects—without changing ring slots. The replay output driver, dynamic wall
graffiti adapter, compositor pixel goldens, and output integration remain to be
connected.

The native GPU boundary now includes a statically validated wgpu 27 shader and
host layouts for temporal multiview sprite drawing. It uploads premultiplied
atlas bytes as an RGBA8 UNORM 2D texture array to preserve the bundled Pixi
WebGL renderer's byte-space filtering and blending, bounds multiview counts to
the configured 2–8 range, and caps the in-flight ring at three batches. Each ring
slot owns its instance/uniform buffers and RGBA texture-array target, and an
owning submission context leases a slot once before submitting the command
buffer. This prevents multiple recorded passes from observing the last queued
upload or overwriting one target. A flat `(view, instance)` storage buffer
renders all active timestamp layers with ordered normal, additive, multiply, and screen
blend runs in one multiview pass. Instances use Pixi-style top-left pixel
coordinates; the shader performs the final output-size-aware clip-space
conversion. Filtered sprites remain isolated in that order and use two
shared temporal scratch arrays for Pixi's quality-four, eight-pass five-tap
blur. Each view reads its independently animated filter strength. The display
object's blend mode applies while rendering the filter input; Pixi's normal
filter state applies on the final pass directly into the scene, avoiding an
extra RGBA8 roundtrip. The three main ring targets plus shared scratch are
bounded together by a 1 GiB allocation limit. In addition to static tests,
the NVIDIA Vulkan smoke path creates these pipelines and submits an empty black
frame, a filtered screen-blended white sprite, and one shared temporal scene
whose overlapping red sprite, green vector, and blue sprite prove cross-kind
display order in the readback. The smoke path also submits a solid temporal
terrain draw, exercises whole-node vector blur, and verifies partial coverage
at a subpixel vector edge after a 2×-per-axis, four-sample supersampling resolve.
Vector meshes now receive immutable
content-derived identities and are deduplicated into one resident vertex bank
when the pipeline is created. Each in-flight vector slot permanently owns its
instance/uniform buffers and bind group, so temporal microbatches upload only
animated instance state. Filtered vectors use the shared temporal ping/pong
arrays for the same four-horizontal/four-vertical quality-four blur as sprites.
The filter-input draw retains the node blend and the final pass uses normal
Pixi filter blending. The supersample array is counted together with sprite
targets and shared scratch under the 1 GiB temporal color limit; resident
geometry plus all vector ring and filter-configuration buffers share a 256 MiB
allocation ceiling. This is submission plumbing, not yet a throughput
claim: stateless action-track compilation, device-level multibatch tests,
object-local pooled filter-frame edge parity, pixel goldens, final compositor
wiring, and GPU-resident encoder interop are still required. A source-complete fallback
output path now converts RGBA8 array targets into full-resolution `R8Unorm` Y
and half-resolution `Rg8Unorm` interleaved UV batches using the BT.709 limited
matrix, strips 256-byte GPU copy-row padding, and streams exact NV12 frames into
one FFmpeg H.264 process. The sink publishes only a successfully flushed file,
uses explicit H.264 VUI color metadata, and never clobbers a concurrently
created destination. This establishes an encoded-output oracle, but the
GPU-to-host readback cannot satisfy the final latency target. Readback creation
checks the device buffer limit, requires portable multiview support, and uses
exclusive mutable access across copy/map/unmap. FFmpeg input writes have a
deadline so a stalled encoder cannot hang the renderer indefinitely. For widths
that already satisfy the 256-byte copy-row alignment (including 2048), the
mapped readback visitor now lends each complete frame directly to the sink:
there is no per-frame allocation or second host copy. Padded widths reuse one
fully overwritten scratch frame. An unmap guard covers success, visitor error,
mapping failure, and panic unwinding.

The initial native draw-node adapters cover root containers, generic
`container` and `sprite` processors, deterministic supersampled premultiplied
`circle` graphics, the retained state-derived `resourceCircle` processor, and
`userBadge`. The badge adapter samples the activation tick's authenticated
global users track, inserts self-contained badge images into the same
content-addressed atlas, preserves the image branch's temporary-scope lifetime
across reruns, and falls back to the official circle payload.
The resource adapter reproduces current-versus-previous resource comparison,
metadata color/radius defaults, capacity scaling, and the early return that
leaves the prior scope node alive. Shared processor resolution also applies the
metadata `path` to current and previous state before payload and action
evaluation. Absolute radius lives on the instance, so
circles with the same fill/stroke proportions share one bounded normalized
atlas asset across a replay; combined resource/procedural atlases remain
content-addressed on disk. A zero-radius result remains an invisible action
target. The retained `draw` adapter now has a bounded typed command compiler
covering the complete observed Pixi Graphics method inventory: fill and line
styles, arcs, circles, ellipses, polygons, rectangles, and rounded rectangles.
It preserves dynamic geometry as compact vector commands instead of creating a
per-activation permanent texture. A bounded CPU lowering stage now reproduces
Pixi's path/style flushing, adaptive arc and primitive subdivision, earcut
polygon fills, and default miter strokes, then feeds a real multiview vector GPU
pipeline. Scene traversal also retains one heterogeneous sprite/vector display
order. Temporal microbatches now topologically merge that order across active
views, pad absent sprite and vector instances, and encode every activation into
one leased target and command submission without grouping across an intervening
drawable kind. Static geometry is hashed once after tessellation, deduplicated
across activations, uploaded once into a bounded resident vertex bank, and
selected by per-draw `instance_index`; only affine transforms, tint, alpha, and
visibility change in the preallocated temporal ring. The `siteProgress`
frontend now also preserves its entity-scope strict progress cache: equal
progress leaves the prior node alive, while changes emit
the official rotated ring and optional clamped filled wedge as the same compact
vector command stream.
The sprite path
preserves intrinsic-versus-raster
dimensions, one-axis aspect sizing, Pixi anchor/pivot behavior, parent alpha and
visibility, tint clamping, and retained blend IDs. Parent transforms are
composed as full affine matrices and submitted directly, retaining shear from
nested nonuniform transforms. The same lifecycle runtime now targets existing
scope/root nodes for `runAction`, skips unreachable action construction and RNG
consumption, and keeps disappearing subtrees pinned through the official
half-transition fade without confusing a reused entity ID. Root containers and
same-spelled JavaScript scope keys remain distinct identities, including in the
stateless composition path. Random-bearing processor payloads remain
fail-closed until their processor-specific lazy evaluation order is lowered;
the retained payloads are pure and keep randomness in typed action trees.

## Tier 2: optional faster Pixi encoder

Electron 39 supports offscreen rendering with `useSharedTexture: true`. On Linux,
its paint event exposes a `nativePixmap` containing DMA-BUF plane file
descriptors, stride, offset, size, and modifier. It can request NV12 output. A
native Node module can therefore:

1. Import the DMA-BUF into Vulkan with its explicit modifier.
2. Keep NV12 frames GPU-resident.
3. Submit them to Vulkan Video encode or a CUDA/NVENC interop path.
4. Mux packets with their real timestamps into fragmented MP4.
5. Release each Electron shared texture only after the GPU fence signals.

This removes the current software encoder/readback cost. It requires a native
module because JavaScript cannot safely own or synchronize DMA-BUF/Vulkan/NVENC
lifetimes. It also requires an explicit render/paint acknowledgement so Electron's
limited shared-texture pool is never overrun.

This tier is worthwhile when a faster Pixi fallback is valuable, but it does not
by itself satisfy two seconds: Pixi still applies and draws frames sequentially.
It is not a prerequisite for the purpose-built temporal renderer, which should
own its Vulkan images directly.

## Tier 3: temporal GPU batching for the strict target

The 6k–15k frame/s goal requires a purpose-built replay renderer rather than a
general scene graph:

1. **Compile replay data.** Decode all chunks, map player IDs once, diff objects,
   and emit a versioned, stateless ReplayIR containing entity lifetimes,
   property segments, finite effects, procedural phase, and counter-based random
   identities. Hash renderer metadata and fail closed on unknown processors or
   actions instead of silently changing the picture.
2. **Cache static geometry.** Terrain, walls, and unchanged structures become
   persistent instanced buffers/texture layers.
3. **Batch time.** A compute shader evaluates object transforms, colors, and
   sprite indices for many timestamps. Render a time batch to a 2D texture array
   with multiview/instancing, rather than issuing the Pixi scene once per frame.
4. **Encode concurrently.** Feed completed array layers through NV12 conversion
   into multiple hardware encoder sessions while the next batch renders.
5. **Preserve semantics.** Use the existing virtual timeline as the reference
   oracle. Golden tests compare selected pixels/object transforms at tick starts,
   midpoints, endpoints, spawn/despawn boundaries, and ticker animation phases.
6. **Shard only from stateless tracks.** Raw-state checkpoints are compiler
   inputs only. Exact independent units start from absolute-time ReplayIR tracks
   carrying every cross-boundary finite effect and procedural phase. Assign each
   timestamp to exactly one half-open unit, render units out of order, and mux
   timestamped IDR fragments in order.

The practical milestone order is:

- ReplayIR compiler, official calculation outputs, and the active
  action/expression core (implemented), then processor graphs with golden
  comparisons against Pixi;
- native ReplayIR validation, typed processor/action planning, lifecycle interval
  compilation, absolute-time frame planning, deterministic atlas
  preparation/cache, and bounded multiview sprite submission (implemented),
  then processor-specific scene lowering and a complete static board renderer;
- temporal Vulkan/wgpu texture-array execution and color goldens;
- direct NV12 conversion plus NVENC or Vulkan Video and timestamp-aware MP4;
- out-of-order unit scheduler, ordered muxing, and throughput tuning;
- optional Electron DMA-BUF bridge for the sequential Pixi fallback.

The current runtime is the oracle and fallback, not throwaway work: it defines
the precise frame/tick contract the native renderer must match.

## Current benchmark prerequisite

The prior validation session had a still-running CachyOS 7.1.3 kernel with
NVIDIA 610.43.02 loaded after packages installed kernel 7.1.5 and NVIDIA
610.43.03. A normal reboot resolved that mismatch: the running kernel, loaded
module, userspace library, NVML, and Vulkan driver now agree on the installed
versions.

A real RTX 5090 wgpu/Vulkan smoke submission on NVIDIA 610.43.03 successfully
created the multiview sprite/blur, vector, terrain, lighting-compositor, and
NV12 pipelines. Three two-layer submissions read back a 64×64 limited-range
black frame with Y=16 and U=V=128, non-black luma from a filtered
screen-blended white sprite through the lighting compositor, and the final blue
center pixel from an overlapping red sprite → green vector → blue sprite scene.
A second offset vector proves nonzero `first_instance`, the result is read from
the second in-flight ring slot, and duplicate mesh registration reports one
resident geometry with six vertices. The AMD RADV path remains an independently
validated fallback. This validates live pipeline creation, heterogeneous
submission order, resident geometry reuse, conversion, and readback; it is not
yet an end-to-end throughput benchmark.

The release `gpu-throughput` probe now exercises the production multiview
sprite targets and the GPU-resident BT.709 NV12 conversion boundary at target
cardinality. On that same RTX 5090, 16,001 2048² frames with 500 visible sprite
instances per frame completed on the GPU in 0.571 s (28,046 frames/s,
117.6 RGBA Gpixels/s), after 0.174 s of device/pipeline allocation. It used six
views per batch, three in-flight batches, and 889 command submissions. This
excludes replay loading, terrain, vectors, filters, hardware encode, and muxing,
so it proves raster/conversion capacity rather than end-to-end completion.

For comparison, the installed FFmpeg/NVENC path encoded 16,001 synthetic black
2048² frames in 27.2 s with `h264_nvenc -preset p1 -tune ull -rc constqp -qp
18`. That measurement includes host-side frame generation and upload and is not
an encoder-engine-only limit, but it confirms that the existing CPU-fed FFmpeg
fallback cannot satisfy the strict latency target. Direct GPU-image interop and
ordered parallel encoder sessions remain necessary.

Parallel encoder probes narrow that constraint further. Eight concurrent
CPU-fed H.264/NVENC segments covering the same 16,001-frame workload completed
in 11.96 s; eight AV1/NVENC segments completed in 11.25 s. FFmpeg's
GPU-resident Vulkan H.264 path encoded 2,281 synthetic 2048² NV12 frames in
5.185 s at its stable default async depth. Raising the async depth to 16 instead
aborted inside FFmpeg's Vulkan encoder on this driver, so Vulkan Video is not a
competitive or sufficiently robust primary path yet.
Those tests are not a formal hardware lower bound, but they show that removing
readback/upload alone cannot plausibly bridge the remaining order-of-magnitude
gap on this machine. The final encoder design therefore needs measured
encoder-engine concurrency and may require different output constraints or
additional encoding hardware to meet two seconds.

A direct SDK probe now removes that uncertainty for resident inputs. It uploads
a bounded 16-frame CUDA NV12 ring once, then times only NVENC submission,
ordered bitstream draining, and EOS. H.264 High/P1/ultra-low-latency/CQP18 took
6.840 s for the 2,281-frame 2048² target (333.5 fps) and occupied about one
third of the reported encoder capacity. The same resident ring encoded valid
AV1 Main low-overhead OBU in 1.942 s (1,174.5 fps); FFprobe and both FFmpeg and
dav1d decoded all 2,281 frames. File output versus discard did not materially
change either result. AV1 therefore crosses the encoder-only two-second line,
while H.264 cannot do so in one session. The end-to-end path must still import
the renderer's exportable Vulkan images as CUDA arrays, register them directly,
overlap conversion with encode, and keep allocation/setup outside the measured
critical path.

The retained 286-tick CTF reference capture produced 2,281 distinct 2048²
frames at 30 fps in 12.777 s (178.5 generated fps). ReplayIR v8 compilation was
263.7 ms, canonical artifact publication was 15.1 ms, and the compatibility
scheduler/render/encode loop was 10.951 s. The resulting 3.38 MB authenticated
artifact now passes the native frontend end to end: 83,139 renderer lifecycle
events resolve into 19,253 activations and 2,574 native scene templates without
reopening the game client. This is the current
full-resolution correctness oracle, not a sub-two-second result.

The native `render-replay` command now supplies the missing correctness-driver
boundary: canonical ReplayIR is resolved into matching temporal scene and
terrain batches, rendered through the production Vulkan pipelines, converted
to tightly packed BT.709-limited NV12, streamed into one H.264 encoder, and
muxed to MP4. A three-frame 64² synthetic ReplayIR completed successfully in
one six-view batch; FFprobe reported H.264, the exact 64² extent, three frames,
limited range, and BT.709. A lossless regression decodes the first frame back to
the exact input NV12 bytes. This also found and fixed missing input-side FFmpeg
color metadata that previously changed Y/U/V values before encoding.

The driver fails before atlas/GPU/encoder/output setup when active processors
lack adapters. `creepBuildBody` now lowers to an ordered multicolor mesh followed
by the renderer's centered 120×120 TOUGH atlas sprite when present, and the nine
captured body-part labels deduplicate to four zoom-aware Roboto raster assets
with global-bound pixel snapping. The
contract-aware decoration path also proves that the oracle's 89 decoration
activations have no object/creep matches and therefore emit nothing; a future
matching decoration contract still fails closed. A captured-subset
`creepActions` adapter now supports all 9,036 runs: 36 first-run no-ops, shared
persistent cover targets, 1,213 flashes, 27 bites, and 224 crisp/blur shot
drawables. Unknown live branches still fail closed.

The complete cached CTF then rendered without touching Steam: all 2,281 frames
at 2048² and 30 fps were muxed into a 76.033-second H.264/BT.709 limited-range
MP4. The optimized correctness run automatically selected four temporal views
to remain below the 512 MiB vector-target budget and took 12.232 seconds total
(1.420 seconds setup, 10.506 seconds render/readback/encode, 217.1 generated
frames/s). This closes
the functional full-video gate. Replacing the blocking readback/single FFmpeg
pipe with measured zero-copy parallel encoding remains the next performance
gate; the strict sub-two-second target is not yet achieved.

## Direct AV1 full-replay path (2026-08-01)

The production driver now implements that zero-copy boundary. Each eight-frame
multiview submission converts directly into dedicated exportable packed-NV12
Vulkan images, which are imported once as CUDA arrays in a bounded AV1/NVENC
ring. Each CUDA array and NVENC resource registration remains resident for the
ring lifetime. A slot is mapped for `NvEncEncodePicture`, kept mapped until its
output bitstream has been locked and copied as required by the SDK, and then
unmapped before Vulkan reuse.
FFmpeg only stream-copies the resulting OBU packets into MP4; no rendered pixels
return to host memory.

The active wgpu Vulkan device exposes external-memory FDs but does not enable
`VK_KHR_external_semaphore_fd`. The driver therefore host-serializes Vulkan
completion before NVENC mapping and NVENC completion before Vulkan reuse. This
is verified on the reference NVIDIA driver, but it is not claimed as a portable
Vulkan/CUDA visibility contract; constructing wgpu on a device with exported
semaphore support remains required for that guarantee.

Exact scene optimizations removed the dominant redundant work. The
vector resolver computes a conservative union of transformed geometry bounds
across active temporal layers, partially clears the 2× scratch array, and
scissors unfiltered downsample/composite passes to that union. Static terrain
lighting is now sampled from its resident cache rather than copied through a
128 MiB eight-layer compositor target for every batch. Renderer lifecycle
events defer detached-target collection to the end of each tick, replacing
thousands of equivalent full-set rebuilds with one. Processor definitions are
resolved once, and active processor/action scopes use borrowed nested hash
lookups instead of allocating ordered tuple keys per event.

Two Vulkan submissions now rotate across separate render, upload, lighting, and
packed-NV12 ring slots; shared scratch accesses remain ordered on the same
Vulkan queue. Batch N+1 is submitted before the host waits specifically for
batch N, so N+1 can remain on the GPU while N enters NVENC. The encoder
keeps a 32-surface input/output ring registered, and FFmpeg packet writes run on
a bounded worker while preserving access-unit order and atomic MP4 publication.

On the RTX 5090 reference system, the corrected persistent-registration
baseline at high-quality CQP18 took 3.190 s for render/encode/mux and produced a
66,667,497-byte MP4. Persistent registration reduced that to about 2.22 s;
tick-batched lifecycle collection and the two-submission GPU schedule reduced
it to about 2.05 s without changing the encoded output.

The final AV1 configuration uses NVENC's explicit three-way split-frame mode at
the same CQP18. NVIDIA documents that split-frame encoding trades some coding
efficiency for single-session throughput, so the complete decoded result was
compared against the prior CQP18 oracle: average PSNR was 50.745 dB (minimum
50.170 dB), and aggregate SSIM was 0.992994. The result contains 66,038,060
encoded OBU bytes in a 66,041,815-byte AV1 Main MP4 with all 2,281 distinct
2048² BT.709-limited frames and lasts
76.033 seconds. The final faststart-enabled run with SDK-required input mappings
held through bitstream lock measured 1.787 seconds for the render/encode/mux
critical path (1,276.4 generated frames/s), plus 1.155 seconds of one-time
process/device/cache setup. The strict sub-two-second critical-path target is
therefore achieved at full resolution, animation cardinality, and the default
high-quality CQP18 setting.
