# Screeps Arena Videoizer

Board-only video capture for Screeps Arena replays in the Steam/Electron client.

The recorder launches Screeps Arena through Steam, opens a replay URL, and hides
the viewer UI. Instead of screen-recording playback in real time, the patched app
**renders the replay deterministically**: it steps through every tick, renders
each frame with PIXI on the GPU, and encodes it to H.264 **in-process with
WebCodecs** — the frame never leaves the GPU, so there is no slow `gl.readPixels`
CPU readback. The H.264 elementary stream is piped through a FIFO to a host
`ffmpeg`, which just remuxes it into MP4 (`-c copy`, no decode/re-encode). The
whole replay is produced as fast as the machine can render and encode —
independent of playback speed and with no fixed load wait (typically several times
faster than real time on a GPU display).

By default it renders on your live, GPU-backed X display — the GPU is what makes
capture fast. A capture window appears while recording (you can minimize it). Add
`--headless` to render into a hidden Xvfb display instead, but Xvfb has no GPU so
WebGL falls back to slow software rendering — that mode is for debugging or
GPU-less machines. See [GPU vs. headless](#gpu-vs-headless).

## Requirements

- Linux
- A GPU-backed X display (your normal desktop session)
- Steam with Screeps Arena installed
- `ffmpeg`
- `Xvfb` (only for `--headless`)
- Node.js, only for the one-time patch script

## Setup

Apply the board-capture patch to the installed Screeps Arena app:

```bash
./patch-screeps-arena.js
```

By default this patches:

```text
/home/marvin/.local/share/Steam/steamapps/common/ScreepsArena
```

Use `SCA_APP_ROOT=/path/to/ScreepsArena ./patch-screeps-arena.js` if your Steam
library is somewhere else. The patcher creates `*.videoizer.bak` backups next to
the modified bundle files.

## Record

```bash
./record-board-x11.sh "https://arena.screeps.com/game/A1CWF344IR" \
  out/A1CWF344IR_board_1920.mp4
```

There is no duration argument: the entire replay is captured. The output defaults
to `1920x1920`, 30 fps H.264 at a 16 Mbps target bitrate. A capture window appears
on your display while recording (you can minimize it).

For debugging or on a machine with no GPU display, render into a hidden Xvfb
instead with `--headless` (much slower — software WebGL):

```bash
./record-board-x11.sh --headless \
  "https://arena.screeps.com/game/A1CWF344IR" \
  out/A1CWF344IR_board_1920.mp4
```

Useful overrides:

```bash
WIDTH=1920
HEIGHT=1920
FPS=30
FRAMES_PER_TICK=8        # frames rendered per game tick (higher = smoother/slower)
BITRATE=16000000         # H.264 target bitrate in bits/s (WebCodecs encoder)
CAPTURE_TIMEOUT=1800     # hard cap (seconds) before giving up
BOARD_ZOOM=0.168
BOARD_PAN_X=-64
BOARD_PAN_Y=-90
```

The output's playback speed is `FPS / FRAMES_PER_TICK` ticks per second (default
`30 / 8 = 3.75 t/s`, close to the in-app viewer default). Each tick spans
`FRAMES_PER_TICK` interpolated frames, so motion is smooth.

The default board preset fits `A1CWF344IR` in a square frame without clipping.
For other arenas, tune `BOARD_ZOOM`, `BOARD_PAN_X`, and `BOARD_PAN_Y`.

## GPU vs. headless

| Mode | Flag | GPU | Window | Speed |
| --- | --- | --- | --- | --- |
| GPU (default) | — | yes (your live X display) | a capture window appears | fast, well above real time |
| Headless | `--headless` | no (Xvfb → software WebGL) | none | slow, often slower than real time |

Capture is render-bound, so the GPU matters. By default the app renders on your
live, GPU-backed X display (`RENDER_DISPLAY`, defaulting to `$DISPLAY`, else `:0`).
Each frame is encoded on the GPU and never read back to the CPU, and the desktop is
never screen-grabbed — so the window only needs to exist; you can minimize or
occlude it and rendering keeps running at full speed. (This relies on launching the
app with `--disable-gpu-vsync` and `--disable-background-timer-throttling`, which
the script always passes — without them an occluded window stalls the renderer.)

`--headless` runs the app in a hidden Xvfb display instead. Xvfb provides no GPU,
so the WebGL renderer falls back to software (SwiftShader/llvmpipe) and a full
replay can take longer than just watching it. Use it only for debugging or on
machines with no GPU-backed display.

## Notes

Each frame is rendered with PIXI and encoded to H.264 in-process with WebCodecs
(`nodeIntegration` is enabled in the client, so the renderer can also touch the
filesystem). `VideoFrame(canvas)` keeps the pixels on the GPU — there is no
`gl.readPixels` readback — and the encoder emits an Annex-B H.264 elementary
stream that is written through a FIFO. The host `ffmpeg` reads that FIFO and
remuxes the stream into MP4 with `-c copy` (no decode, no re-encode). The FIFO and
its sidecar files live next to the output under `$HOME`, because Screeps Arena runs
inside Steam's pressure-vessel container and can only reach paths the container
shares (the home directory) — the host `ffmpeg` is not visible from inside it. The
script does not use `x11grab` and never captures the desktop or visible Steam
client; the X display (your live GPU display by default, or a hidden Xvfb with
`--headless`) only provides the WebGL context the renderer needs. The temporary
`/tmp/screeps-arena-board-capture` sentinel is created only while recording so the
patched app can size its Electron window for board capture; the per-run
`*.cap.meta` file hands the frame geometry to the script, and the
`*.cap.done`/`*.cap.error` sentinels signal completion or failure.

Animation note: positional motion (creeps, structures, projectiles) is driven by
the renderer's action system and is reproduced exactly. Purely decorative,
ticker-driven texture animations (e.g. animated swamp tiles) are not advanced
during deterministic capture.

