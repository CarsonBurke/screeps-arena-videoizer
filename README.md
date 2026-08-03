# Screeps Arena Videoizer

Deterministic board-only video generation for Screeps Arena replays.

Drives the patched Pixi renderer from a virtual clock, applies replay states
programmatically, and muxes H.264 into MP4 via host `ffmpeg`. Not a real-time
screen capture.

## Requirements

- Linux with a GPU-backed X display
- Steam or Flatpak Steam with Screeps Arena installed
- `ffmpeg`, `ffprobe`, Node.js
- `Xvfb` only for optional slow `--headless` debugging

## Setup

```bash
npm run check
./patch-screeps-arena.js
```

The patcher auto-detects native and Flatpak Steam installs. Override with
`SCA_APP_ROOT=/path/to/ScreepsArena`. Re-running is safe; it keeps
`*.videoizer.bak` backups.

## Generate a video

```bash
./record-board-x11.sh "https://arena.screeps.com/game/A1CWF344IR" \
  out/A1CWF344IR.mp4
```

Defaults: 2048×2048, 30 fps, 3.75 ticks/s (8 frames/tick), 60 Hz simulation,
full-map auto-fit with 32px padding, 24 Mbps H.264.

```bash
WIDTH=2048 HEIGHT=2048 FPS=30 \
TICKS_PER_SECOND=3.75 SIMULATION_FPS=60 \
BITRATE=24000000 \
./record-board-x11.sh <replay> out/video.mp4
```

`TICKS_PER_SECOND` sets apparent playback speed. If omitted, speed is
`FPS / FRAMES_PER_TICK` (default FPT=8 → 3.75 ticks/s at 30 fps).

### Common settings

| Variable | Default | Notes |
|---|---|---|
| `BOARD_ZOOM` | `auto` | Fits the full 100×100 map; a numeric zoom may crop |
| `BOARD_PADDING` | `32` | Pixel padding around the board |
| `BOARD_PAN_X` / `BOARD_PAN_Y` | `0` | Pixel offsets after centering |
| `PRELOAD_CONCURRENCY` | `4` | Replay chunk fetch concurrency |
| `CAPTURE_TIMEOUT` | `1800` | Seconds |
| `CLOSE_APP_AFTER_CAPTURE` | `0` | Keep app open for reuse; set `1` for one-shot |
| `RENDER_DISPLAY` | `:0` | X display |
| `REPLAY_IR` | off | Retain ReplayIR + renderer contract next to telemetry |

Live captures reuse the open Steam/Electron session via deep-link. Headless
always closes the app (its private Xvfb is torn down). Prefer live capture;
`--headless` is much slower without hardware WebGL.

## Outputs

MP4 is published only after the completion marker, telemetry, and decoded frame
count all agree.

Per-run logs and telemetry:

- Native Steam: `~/.cache/screeps-arena-videoizer/`
- Flatpak Steam:
  `~/.var/app/com.valvesoftware.Steam/cache/screeps-arena-videoizer/`

## Native renderer

`native-renderer/` is a Rust path that consumes canonical ReplayIR and renders
without Steam. See [PERFORMANCE.md](PERFORMANCE.md) for architecture and
benchmarks.

```bash
# Hardware smoke (Vulkan pipelines + readback)
cargo run --release --manifest-path native-renderer/Cargo.toml --bin gpu-smoke

# ReplayIR → MP4 (default: AV1 via NVENC; --h264 / --software as fallbacks)
cargo run --release --manifest-path native-renderer/Cargo.toml \
  --bin render-replay -- /path/to/capture.replay-ir.json out/native.mp4 --overwrite
```

## Tests

```bash
npm test
```
