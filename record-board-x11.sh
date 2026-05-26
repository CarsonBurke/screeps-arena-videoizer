#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  ./record-board-x11.sh [--headless] <replay-url-or-route> <output.mp4>

Renders the whole replay deterministically (not in real time): the patched app
steps the replay tick-by-tick, renders each frame with PIXI on the GPU, and
encodes it in-process to H.264 with WebCodecs (the frame never leaves the GPU,
avoiding the slow gl.readPixels CPU readback). The H.264 stream is piped through
a FIFO; this host script runs ffmpeg as the reader and just remuxes it to MP4
(-c copy, no re-encode). Recording finishes as fast as the machine can render and
encode, independent of playback speed and with no fixed load wait.

A FIFO is used (instead of letting the app run ffmpeg) because Screeps Arena runs
inside Steam's pressure-vessel container, which cannot reach the host ffmpeg. The
FIFO/meta files live next to the output, under $HOME, which the container shares.

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
  WIDTH=1920 HEIGHT=1920 FPS=30 FRAMES_PER_TICK=8
  BITRATE=16000000               # H.264 target bitrate (bits/s)
  CAPTURE_TIMEOUT=1800            # hard cap (seconds) before giving up
  BOARD_ZOOM=0.168 BOARD_PAN_X=-64 BOARD_PAN_Y=-90
  RENDER_DISPLAY=:0              # GPU X display used for normal (non-headless) runs
  APP=/home/marvin/.local/share/Steam/steamapps/common/ScreepsArena/screeps_arena

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

APP=${APP:-/home/marvin/.local/share/Steam/steamapps/common/ScreepsArena/screeps_arena}
WIDTH=${WIDTH:-1920}
HEIGHT=${HEIGHT:-1920}
FPS=${FPS:-30}
FRAMES_PER_TICK=${FRAMES_PER_TICK:-8}
BITRATE=${BITRATE:-16000000}
CAPTURE_TIMEOUT=${CAPTURE_TIMEOUT:-1800}
BOARD_ZOOM=${BOARD_ZOOM:-0.168}
BOARD_PAN_X=${BOARD_PAN_X:--64}
BOARD_PAN_Y=${BOARD_PAN_Y:--90}
# GPU X display for normal runs: prefer the caller's live display, fall back to :0.
RENDER_DISPLAY=${RENDER_DISPLAY:-${DISPLAY:-:0}}
# Extra Chromium/Electron flags (space-separated), e.g. to enable GPU rendering.
EXTRA_FLAGS=${EXTRA_FLAGS:-}

if [[ ! -x "$APP" ]]; then
  echo "Screeps Arena executable not found: $APP" >&2
  exit 1
fi

if ! command -v ffmpeg >/dev/null 2>&1; then
  echo "ffmpeg not found on PATH" >&2
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
TARGET=$(append_param "$TARGET" "replay-expanded=true")
TARGET=$(append_param "$TARGET" "board-capture=true")
TARGET=$(append_param "$TARGET" "board-zoom=$BOARD_ZOOM")
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

# FIFO and sidecars live next to the output (under $HOME) so the container shares them.
FIFO="${OUT}.cap.fifo"
META="${OUT}.cap.meta"
DONE_FILE="${OUT}.cap.done"
ERROR_FILE="${OUT}.cap.error"
FFMPEG_LOG="/tmp/screeps-arena-video-ffmpeg.log"
rm -f "$FIFO" "$META" "$DONE_FILE" "$ERROR_FILE"

cleanup() {
  if [[ -n "${FFMPEG_PID:-}" ]]; then
    kill "$FFMPEG_PID" >/dev/null 2>&1 || true
  fi
  if [[ -n "${APP_PID:-}" ]]; then
    kill "$APP_PID" >/dev/null 2>&1 || true
  fi
  pkill -f '/ScreepsArena/screeps_arena' >/dev/null 2>&1 || true
  if [[ -n "${XVFB_PID:-}" ]]; then
    kill "$XVFB_PID" >/dev/null 2>&1 || true
  fi
  rm -f /tmp/screeps-arena-board-capture "$FIFO" "$META" "$DONE_FILE" "$ERROR_FILE"
}
trap cleanup EXIT

report_error() {
  echo "Capture failed: $1" >&2
  [[ -f "$ERROR_FILE" ]] && { echo "--- app error ---" >&2; cat "$ERROR_FILE" >&2; }
  echo "--- app log tail ---" >&2; tail -n 40 /tmp/screeps-arena-video-app.log 2>/dev/null >&2 || true
  [[ -f "$FFMPEG_LOG" ]] && { echo "--- ffmpeg log tail ---" >&2; tail -n 20 "$FFMPEG_LOG" >&2 || true; }
}

mkfifo "$FIFO"

touch /tmp/screeps-arena-board-capture
MODE_FLAGS=""
if (( HEADLESS )); then
  echo "Rendering headless via Xvfb on $DISPLAY_ID (software WebGL, no GPU — slow; for debugging)." >&2
  Xvfb "$DISPLAY_ID" -screen 0 "${WIDTH}x${HEIGHT}x24" -nolisten tcp >/tmp/screeps-arena-video-xvfb.log 2>&1 &
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
  SCREEPS_ARENA_BOARD_CAPTURE_PAN_X="$BOARD_PAN_X" \
  SCREEPS_ARENA_BOARD_CAPTURE_PAN_Y="$BOARD_PAN_Y" \
  SCREEPS_ARENA_BOARD_CAPTURE_WIDTH="$WIDTH" \
  SCREEPS_ARENA_BOARD_CAPTURE_HEIGHT="$HEIGHT" \
  SCREEPS_ARENA_BOARD_CAPTURE_FIFO="$FIFO" \
  SCREEPS_ARENA_BOARD_CAPTURE_META="$META" \
  SCREEPS_ARENA_BOARD_CAPTURE_FPS="$FPS" \
  SCREEPS_ARENA_BOARD_CAPTURE_FRAMES_PER_TICK="$FRAMES_PER_TICK" \
  SCREEPS_ARENA_BOARD_CAPTURE_BITRATE="$BITRATE" \
  SCREEPS_ARENA_BOARD_CAPTURE_DONE="$DONE_FILE" \
  SCREEPS_ARENA_BOARD_CAPTURE_ERROR="$ERROR_FILE" \
  steam -applaunch 1137320 \
  --ozone-platform=x11 \
  --window-size="${WIDTH},${HEIGHT}" \
  --force-device-scale-factor=1 \
  --disable-background-timer-throttling \
  --disable-gpu-vsync \
  ${MODE_FLAGS} \
  ${EXTRA_FLAGS} \
  "$TARGET" >/tmp/screeps-arena-video-app.log 2>&1 &
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
echo "Muxing H.264 ${VW}x${VH} @ ${VFPS}fps -> $OUT" >&2

# Phase 2: start ffmpeg as the FIFO reader; this unblocks the renderer's writer.
# The app already encodes each frame to H.264 in-process (WebCodecs, on the GPU),
# so the FIFO carries an Annex-B elementary stream. ffmpeg just remuxes it into
# MP4 with -c copy (no decode/re-encode). The raw stream has no timing, so set the
# frame rate on input and let the muxer generate timestamps.
ffmpeg -y -fflags +genpts -r "$VFPS" -f h264 -i "$FIFO" -an \
  -c:v copy -movflags +faststart "$OUT" >"$FFMPEG_LOG" 2>&1 &
FFMPEG_PID=$!

# Phase 3: wait for the renderer to finish (done sentinel) or fail.
ffmpeg_reaped=0
while [[ ! -f "$DONE_FILE" ]]; do
  if [[ -f "$ERROR_FILE" ]]; then report_error "renderer error during streaming"; exit 1; fi
  if ! kill -0 "$FFMPEG_PID" 2>/dev/null; then
    # ffmpeg exited. A clean (rc==0) exit means it reached EOF and finalized the
    # MP4 -- i.e. the app closed the FIFO because capture finished. The done
    # sentinel races with ffmpeg's EOF, so a clean exit is success even if the
    # sentinel is not visible yet. Only a nonzero exit is a real failure.
    rc=0; wait "$FFMPEG_PID" || rc=$?; ffmpeg_reaped=1
    if (( rc == 0 )); then break; fi
    report_error "ffmpeg exited early (code $rc)"; exit 1
  fi
  if (( elapsed >= CAPTURE_TIMEOUT )); then report_error "timed out during capture"; exit 1; fi
  sleep 1; elapsed=$((elapsed + 1))
done

# Renderer closed the FIFO; let ffmpeg finalize the file (unless already reaped above).
if (( ! ffmpeg_reaped )); then
  rc=0; wait "$FFMPEG_PID" || rc=$?
  if (( rc != 0 )); then report_error "ffmpeg failed (code $rc)"; exit 1; fi
fi

echo "Done: $OUT" >&2
exit 0
