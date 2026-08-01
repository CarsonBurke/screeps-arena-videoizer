#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  ./record-board-x11.sh [--headless] <replay-url-or-route> <output.mp4>

Renders the whole replay deterministically (not in real time): the patched app
steps the replay tick-by-tick, renders each frame with PIXI on the GPU, and
encodes it in-process to H.264 with WebCodecs. The H.264 stream is piped through
a FIFO; this host script runs ffmpeg as the reader and just remuxes it to MP4
(-c copy, no re-encode). Generation runs as fast as the renderer and available
WebCodecs backend allow, independent of playback speed and with no fixed load wait.

A FIFO is used (instead of letting the app run ffmpeg) because Screeps Arena runs
inside Steam's pressure-vessel container, which cannot reach the host ffmpeg.
Transport files use a per-run cache directory shared with the Flatpak container.

By default the app renders on your live, GPU-backed X display ($RENDER_DISPLAY,
default $DISPLAY or :0). The GPU is what makes capture fast, so this is the normal
mode. A capture window appears while recording; you can minimize it (rendering
does not throttle when hidden).

Pass --headless to render into a hidden Xvfb display instead. Xvfb has no GPU, so
the WebGL renderer falls back to software and capture is slow (often slower than
real time) — use it only for debugging or where no GPU display is available.

Options:
  --headless          Render in a hidden Xvfb display (no window, no GPU, slow).
                      For debugging / headless boxes. Default renders on the GPU.

Environment overrides:
  WIDTH=2048 HEIGHT=2048 FPS=30 FRAMES_PER_TICK=8
  TICKS_PER_SECOND=3.75          # preferred direct playback-speed control
  SIMULATION_FPS=60              # fixed animation/action substep rate
  BITRATE=24000000               # H.264 target bitrate (bits/s)
  CAPTURE_TIMEOUT=1800            # hard cap (seconds) before giving up
  BOARD_ZOOM=auto BOARD_PADDING=32 BOARD_PAN_X=0 BOARD_PAN_Y=0
  PRELOAD_CONCURRENCY=4           # concurrent replay/visual chunk requests
  COMPILER_UNIT_TICKS=50          # ReplayIR planning boundary; Pixi stays serial
  REPLAY_IR=0                     # retain lossless ReplayIR + renderer contract
  CAPTURE_RANDOM_SEED=...         # defaults to the normalized replay route
  RENDER_DISPLAY=:0              # GPU X display used for normal (non-headless) runs
  APP=/path/to/ScreepsArena/screeps_arena  # auto-detected for native/Flatpak Steam

Effective playback speed of the output is FPS / FRAMES_PER_TICK ticks per second
(default 30 / 8 = 3.75 t/s, close to the in-app viewer default).
USAGE
}

HEADLESS=0
POSITIONAL=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --headless) HEADLESS=1; shift ;;
    --) shift; while [[ $# -gt 0 ]]; do POSITIONAL+=("$1"); shift; done ;;
    -*) echo "Unknown option: $1" >&2; usage; exit 1 ;;
    *) POSITIONAL+=("$1"); shift ;;
  esac
done

if [[ ${#POSITIONAL[@]} -lt 2 ]]; then
  usage
  exit 0
fi

TARGET=${POSITIONAL[0]}
OUT=${POSITIONAL[1]}

NATIVE_APP="$HOME/.local/share/Steam/steamapps/common/ScreepsArena/screeps_arena"
FLATPAK_STEAM_HOME="$HOME/.var/app/com.valvesoftware.Steam"
FLATPAK_APP="$FLATPAK_STEAM_HOME/.local/share/Steam/steamapps/common/ScreepsArena/screeps_arena"
if [[ -z "${APP:-}" ]]; then
  if [[ -x "$NATIVE_APP" ]]; then
    APP=$NATIVE_APP
  elif [[ -x "$FLATPAK_APP" ]]; then
    APP=$FLATPAK_APP
  else
    APP=$NATIVE_APP
  fi
fi
WIDTH=${WIDTH:-2048}
HEIGHT=${HEIGHT:-2048}
FPS=${FPS:-30}
FRAMES_PER_TICK=${FRAMES_PER_TICK:-8}
TICKS_PER_SECOND=${TICKS_PER_SECOND:-}
SIMULATION_FPS=${SIMULATION_FPS:-60}
BITRATE=${BITRATE:-24000000}
CAPTURE_TIMEOUT=${CAPTURE_TIMEOUT:-1800}
BOARD_ZOOM=${BOARD_ZOOM:-auto}
BOARD_PADDING=${BOARD_PADDING:-32}
BOARD_PAN_X=${BOARD_PAN_X:-0}
BOARD_PAN_Y=${BOARD_PAN_Y:-0}
PRELOAD_CONCURRENCY=${PRELOAD_CONCURRENCY:-4}
COMPILER_UNIT_TICKS=${COMPILER_UNIT_TICKS:-50}
REPLAY_IR=${REPLAY_IR:-0}
# GPU X display for normal runs: prefer the caller's live display, fall back to :0.
RENDER_DISPLAY=${RENDER_DISPLAY:-${DISPLAY:-:0}}
# Extra Chromium/Electron flags (space-separated), e.g. to enable GPU rendering.
EXTRA_FLAGS=${EXTRA_FLAGS:-}

if [[ ! -x "$APP" ]]; then
  echo "Screeps Arena executable not found: $APP" >&2
  exit 1
fi

for dependency in ffmpeg ffprobe node; do
  if ! command -v "$dependency" >/dev/null 2>&1; then
    echo "$dependency not found on PATH" >&2
    exit 1
  fi
done

if [[ "$APP" == "$FLATPAK_STEAM_HOME"/* ]]; then
  STEAM_MODE=flatpak
elif command -v steam >/dev/null 2>&1; then
  STEAM_MODE=native
else
  STEAM_MODE=flatpak
fi

if [[ "$STEAM_MODE" == native ]] && command -v steam >/dev/null 2>&1; then
  STEAM_LAUNCH=(steam -applaunch 1137320)
elif [[ "$STEAM_MODE" == flatpak ]] && command -v flatpak >/dev/null 2>&1 \
  && flatpak info com.valvesoftware.Steam >/dev/null 2>&1; then
  STEAM_LAUNCH=(flatpak run com.valvesoftware.Steam -applaunch 1137320)
else
  echo "The Steam installation matching $APP is not available" >&2
  exit 1
fi

normalize_target() {
  local target=$1
  if [[ "$target" == screeps-arena://* ]]; then
    printf '%s' "$target"
  elif [[ "$target" == https://arena.screeps.com/* ]]; then
    printf 'screeps-arena://%s' "${target#https://arena.screeps.com/}"
  elif [[ "$target" == http://arena.screeps.com/* ]]; then
    printf 'screeps-arena://%s' "${target#http://arena.screeps.com/}"
  else
    printf 'screeps-arena://%s' "${target#/}"
  fi
}

append_param() {
  local target=$1
  local param=$2
  if [[ "$target" == *\?* ]]; then
    printf '%s&%s' "$target" "$param"
  else
    printf '%s?%s' "$target" "$param"
  fi
}

encode_query_value() {
  node -e 'process.stdout.write(encodeURIComponent(process.argv[1]))' "$1"
}

find_display() {
  local n
  for n in $(seq 120 199); do
    if [[ ! -e "/tmp/.X${n}-lock" ]]; then
      printf ':%s' "$n"
      return
    fi
  done
  echo "No free Xvfb display found" >&2
  exit 1
}

OUT=$(realpath -m "$OUT")
mkdir -p "$(dirname "$OUT")"

TARGET=$(normalize_target "$TARGET")
CAPTURE_RANDOM_SEED=${CAPTURE_RANDOM_SEED:-${TARGET%%\?*}}
CAPTURE_RANDOM_SEED_ENCODED=$(encode_query_value "$CAPTURE_RANDOM_SEED")
TARGET=$(append_param "$TARGET" "replay-expanded=true")
TARGET=$(append_param "$TARGET" "board-capture=true")
TARGET=$(append_param "$TARGET" "board-zoom=$BOARD_ZOOM")
TARGET=$(append_param "$TARGET" "board-padding=$BOARD_PADDING")
TARGET=$(append_param "$TARGET" "board-pan-x=$BOARD_PAN_X")
TARGET=$(append_param "$TARGET" "board-pan-y=$BOARD_PAN_Y")

XVFB_PID=
APP_PID=
FFMPEG_PID=
if (( HEADLESS )); then
  DISPLAY_ID=${XVFB_DISPLAY:-$(find_display)}
else
  DISPLAY_ID=$RENDER_DISPLAY
fi

# Flatpak remaps its private host data directory to the application's normal
# $HOME. Keep separate host/app paths so both Electron and the host ffmpeg see
# the same per-run transport files.
CAPTURE_ID="capture-$$"
if [[ "$STEAM_MODE" == flatpak ]]; then
  HOST_TRANSPORT_DIR="$FLATPAK_STEAM_HOME/cache/screeps-arena-videoizer"
  APP_TRANSPORT_DIR="$HOME/.cache/screeps-arena-videoizer"
else
  HOST_TRANSPORT_DIR="$HOME/.cache/screeps-arena-videoizer"
  APP_TRANSPORT_DIR=$HOST_TRANSPORT_DIR
fi
mkdir -p "$HOST_TRANSPORT_DIR"
FIFO="${HOST_TRANSPORT_DIR}/${CAPTURE_ID}.fifo"
META="${HOST_TRANSPORT_DIR}/${CAPTURE_ID}.meta"
DONE_FILE="${HOST_TRANSPORT_DIR}/${CAPTURE_ID}.done"
ERROR_FILE="${HOST_TRANSPORT_DIR}/${CAPTURE_ID}.error"
DEBUG_FILE="${HOST_TRANSPORT_DIR}/${CAPTURE_ID}.debug.log"
TELEMETRY_FILE="${HOST_TRANSPORT_DIR}/${CAPTURE_ID}.telemetry.json"
APP_FIFO="${APP_TRANSPORT_DIR}/${CAPTURE_ID}.fifo"
APP_META="${APP_TRANSPORT_DIR}/${CAPTURE_ID}.meta"
APP_DONE_FILE="${APP_TRANSPORT_DIR}/${CAPTURE_ID}.done"
APP_ERROR_FILE="${APP_TRANSPORT_DIR}/${CAPTURE_ID}.error"
APP_DEBUG_FILE="${APP_TRANSPORT_DIR}/${CAPTURE_ID}.debug.log"
APP_TELEMETRY_FILE="${APP_TRANSPORT_DIR}/${CAPTURE_ID}.telemetry.json"
FFMPEG_LOG="${HOST_TRANSPORT_DIR}/${CAPTURE_ID}.ffmpeg.log"
PART_OUT="${OUT}.partial-${CAPTURE_ID}.mp4"
rm -f "$FIFO" "$META" "$DONE_FILE" "$ERROR_FILE" "$TELEMETRY_FILE" "$PART_OUT"

# The replay URL is the authoritative per-run configuration. Steam may already
# be running and can otherwise launch Electron with a stale environment.
TARGET=$(append_param "$TARGET" "capture-id=$CAPTURE_ID")
TARGET=$(append_param "$TARGET" "capture-width=$WIDTH")
TARGET=$(append_param "$TARGET" "capture-height=$HEIGHT")
TARGET=$(append_param "$TARGET" "capture-fps=$FPS")
TARGET=$(append_param "$TARGET" "capture-frames-per-tick=$FRAMES_PER_TICK")
TARGET=$(append_param "$TARGET" "capture-simulation-fps=$SIMULATION_FPS")
TARGET=$(append_param "$TARGET" "capture-bitrate=$BITRATE")
TARGET=$(append_param "$TARGET" "capture-preload-concurrency=$PRELOAD_CONCURRENCY")
TARGET=$(append_param "$TARGET" "capture-compiler-unit-ticks=$COMPILER_UNIT_TICKS")
TARGET=$(append_param "$TARGET" "capture-replay-ir=$REPLAY_IR")
TARGET=$(append_param "$TARGET" "capture-random-seed=$CAPTURE_RANDOM_SEED_ENCODED")
if [[ -n "$TICKS_PER_SECOND" ]]; then
  TARGET=$(append_param "$TARGET" "capture-ticks-per-second=$TICKS_PER_SECOND")
fi

cleanup() {
  if [[ -n "${FFMPEG_PID:-}" ]]; then
    kill "$FFMPEG_PID" >/dev/null 2>&1 || true
  fi
  if [[ -n "${APP_PID:-}" ]]; then
    kill "$APP_PID" >/dev/null 2>&1 || true
  fi
  pkill -f "/ScreepsArena/screeps_arena.*capture-id=$CAPTURE_ID" >/dev/null 2>&1 || true
  if [[ -n "${XVFB_PID:-}" ]]; then
    kill "$XVFB_PID" >/dev/null 2>&1 || true
  fi
  rm -f "$FIFO" "$META" "$DONE_FILE" "$ERROR_FILE" "$PART_OUT"
}
trap cleanup EXIT

APP_LOG="${HOST_TRANSPORT_DIR}/${CAPTURE_ID}.app.log"
XVFB_LOG="${HOST_TRANSPORT_DIR}/${CAPTURE_ID}.xvfb.log"

report_error() {
  echo "Capture failed: $1" >&2
  [[ -f "$ERROR_FILE" ]] && { echo "--- app error ---" >&2; cat "$ERROR_FILE" >&2; }
  echo "--- app log tail ---" >&2; tail -n 40 "$APP_LOG" 2>/dev/null >&2 || true
  [[ -f "$FFMPEG_LOG" ]] && { echo "--- ffmpeg log tail ---" >&2; tail -n 20 "$FFMPEG_LOG" >&2 || true; }
}

mkfifo "$FIFO"

MODE_FLAGS=""
if (( HEADLESS )); then
  echo "Rendering headless via Xvfb on $DISPLAY_ID (software WebGL, no GPU — slow; for debugging)." >&2
  Xvfb "$DISPLAY_ID" -screen 0 "${WIDTH}x${HEIGHT}x24" -nolisten tcp >"$XVFB_LOG" 2>&1 &
  XVFB_PID=$!
  sleep 1
else
  echo "Rendering on GPU display $DISPLAY_ID (a capture window will appear; you may minimize it)." >&2
  # Keep the renderer at full speed even if the window is hidden/occluded/minimized.
  MODE_FLAGS="--disable-renderer-backgrounding --disable-backgrounding-occluded-windows --disable-frame-rate-limit"
fi

DISPLAY="$DISPLAY_ID" \
  env -u WAYLAND_DISPLAY \
  XDG_SESSION_TYPE=x11 \
  SCREEPS_ARENA_DISABLE_DISCORD_RPC=1 \
  SCREEPS_ARENA_BOARD_CAPTURE=1 \
  SCREEPS_ARENA_BOARD_CAPTURE_ZOOM="$BOARD_ZOOM" \
  SCREEPS_ARENA_BOARD_CAPTURE_PADDING="$BOARD_PADDING" \
  SCREEPS_ARENA_BOARD_CAPTURE_PAN_X="$BOARD_PAN_X" \
  SCREEPS_ARENA_BOARD_CAPTURE_PAN_Y="$BOARD_PAN_Y" \
  SCREEPS_ARENA_BOARD_CAPTURE_WIDTH="$WIDTH" \
  SCREEPS_ARENA_BOARD_CAPTURE_HEIGHT="$HEIGHT" \
  SCREEPS_ARENA_BOARD_CAPTURE_FIFO="$APP_FIFO" \
  SCREEPS_ARENA_BOARD_CAPTURE_META="$APP_META" \
  SCREEPS_ARENA_BOARD_CAPTURE_FPS="$FPS" \
  SCREEPS_ARENA_BOARD_CAPTURE_FRAMES_PER_TICK="$FRAMES_PER_TICK" \
  SCREEPS_ARENA_BOARD_CAPTURE_TICKS_PER_SECOND="$TICKS_PER_SECOND" \
  SCREEPS_ARENA_BOARD_CAPTURE_SIMULATION_FPS="$SIMULATION_FPS" \
  SCREEPS_ARENA_BOARD_CAPTURE_BITRATE="$BITRATE" \
  SCREEPS_ARENA_BOARD_CAPTURE_PRELOAD_CONCURRENCY="$PRELOAD_CONCURRENCY" \
  SCREEPS_ARENA_BOARD_CAPTURE_COMPILER_UNIT_TICKS="$COMPILER_UNIT_TICKS" \
  SCREEPS_ARENA_BOARD_CAPTURE_REPLAY_IR="$REPLAY_IR" \
  SCREEPS_ARENA_BOARD_CAPTURE_DONE="$APP_DONE_FILE" \
  SCREEPS_ARENA_BOARD_CAPTURE_ERROR="$APP_ERROR_FILE" \
  SCREEPS_ARENA_BOARD_CAPTURE_DEBUG="$APP_DEBUG_FILE" \
  SCREEPS_ARENA_BOARD_CAPTURE_TELEMETRY="$APP_TELEMETRY_FILE" \
  "${STEAM_LAUNCH[@]}" \
  --ozone-platform=x11 \
  --window-size="${WIDTH},${HEIGHT}" \
  --force-device-scale-factor=1 \
  --disable-background-timer-throttling \
  --disable-gpu-vsync \
  ${MODE_FLAGS} \
  ${EXTRA_FLAGS} \
  "$TARGET" >"$APP_LOG" 2>&1 &
APP_PID=$!

echo "Rendering replay to $OUT (timeout ${CAPTURE_TIMEOUT}s)..." >&2

elapsed=0

# Phase 1: wait for the renderer to announce frame geometry.
while [[ ! -f "$META" ]]; do
  if [[ -f "$ERROR_FILE" ]]; then report_error "renderer error before streaming"; exit 1; fi
  if (( elapsed >= CAPTURE_TIMEOUT )); then report_error "timed out waiting for renderer"; exit 1; fi
  sleep 1; elapsed=$((elapsed + 1))
done

read -r VW VH VFPS < "$META" || true
VW=${VW:-$WIDTH}; VH=${VH:-$HEIGHT}; VFPS=${VFPS:-$FPS}
echo "Muxing H.264 ${VW}x${VH} @ ${VFPS}fps -> $PART_OUT" >&2

# Phase 2: start ffmpeg as the FIFO reader; this unblocks the renderer's writer.
# The app already encodes each frame to H.264 in-process with WebCodecs,
# so the FIFO carries an Annex-B elementary stream. ffmpeg just remuxes it into
# MP4 with -c copy (no decode/re-encode). The raw stream has no timing, so set the
# frame rate on input and let the muxer generate timestamps.
ffmpeg -y -fflags +genpts -r "$VFPS" -f h264 -i "$FIFO" -an \
  -c:v copy -movflags +faststart "$PART_OUT" >"$FFMPEG_LOG" 2>&1 &
FFMPEG_PID=$!

# Phase 3: wait for the renderer to finish (done sentinel) or fail.
ffmpeg_reaped=0
ffmpeg_eof_wait=0
while [[ ! -f "$DONE_FILE" ]]; do
  if [[ -f "$ERROR_FILE" ]]; then report_error "renderer error during streaming"; exit 1; fi
  if (( ! ffmpeg_reaped )) && ! kill -0 "$FFMPEG_PID" 2>/dev/null; then
    # FIFO EOF can precede the renderer's done sentinel by a moment. Reap ffmpeg,
    # but do not call the capture successful until the renderer has also written
    # valid telemetry and its explicit completion marker.
    rc=0; wait "$FFMPEG_PID" || rc=$?; ffmpeg_reaped=1
    if (( rc != 0 )); then report_error "ffmpeg exited early (code $rc)"; exit 1; fi
  fi
  if (( ffmpeg_reaped )); then
    ffmpeg_eof_wait=$((ffmpeg_eof_wait + 1))
    if (( ffmpeg_eof_wait > 10 )); then
      report_error "renderer closed the video stream without a completion marker"
      exit 1
    fi
  fi
  if (( elapsed >= CAPTURE_TIMEOUT )); then report_error "timed out during capture"; exit 1; fi
  sleep 1; elapsed=$((elapsed + 1))
done

# Renderer closed the FIFO; let ffmpeg finalize the file (unless already reaped above).
if (( ! ffmpeg_reaped )); then
  rc=0; wait "$FFMPEG_PID" || rc=$?
  if (( rc != 0 )); then report_error "ffmpeg failed (code $rc)"; exit 1; fi
fi

if [[ ! -f "$TELEMETRY_FILE" ]]; then
  report_error "renderer completed without telemetry"
  exit 1
fi
telemetry_values=$(node -e '
  const t = require(process.argv[1]);
  if (t.ok !== true || !Number.isSafeInteger(t.counts?.expected) || t.counts.expected < 1) process.exit(2);
  if (!Number.isFinite(t.throughputFps) || t.throughputFps < 0) process.exit(3);
  process.stdout.write(`${t.counts.expected} ${t.throughputFps.toFixed(1)}`);
' "$TELEMETRY_FILE") || { report_error "renderer telemetry is invalid"; exit 1; }
read -r expected_frames throughput <<< "$telemetry_values"
actual_frames=$(ffprobe -v error -count_frames -select_streams v:0 \
  -show_entries stream=nb_read_frames -of default=nw=1:nk=1 "$PART_OUT")
if [[ "$actual_frames" != "$expected_frames" ]]; then
  report_error "frame-count mismatch: telemetry expected $expected_frames, MP4 contains $actual_frames"
  exit 1
fi

# Publish only a fully completed and frame-validated MP4. The temporary file is
# in the destination directory, so rename is atomic on the target filesystem.
mv -f -- "$PART_OUT" "$OUT"
echo "Done: $OUT" >&2
echo "Validated $actual_frames frames at ${throughput} generated fps; telemetry: $TELEMETRY_FILE" >&2
exit 0
