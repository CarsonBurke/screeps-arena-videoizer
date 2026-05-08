#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  ./record-board-x11.sh <replay-url-or-route> <output.mp4> [duration-seconds]

Environment overrides:
  WIDTH=1920 HEIGHT=1920 FPS=30 LOAD_WAIT=45
  BOARD_ZOOM=0.168 BOARD_PAN_X=-64 BOARD_PAN_Y=-90
  APP=/home/marvin/.local/share/Steam/steamapps/common/ScreepsArena/screeps_arena
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" || $# -lt 2 ]]; then
  usage
  exit 0
fi

TARGET=$1
OUT=$2
DURATION=${3:-20}

APP=${APP:-/home/marvin/.local/share/Steam/steamapps/common/ScreepsArena/screeps_arena}
WIDTH=${WIDTH:-1920}
HEIGHT=${HEIGHT:-1920}
FPS=${FPS:-30}
LOAD_WAIT=${LOAD_WAIT:-45}
BOARD_ZOOM=${BOARD_ZOOM:-0.168}
BOARD_PAN_X=${BOARD_PAN_X:--64}
BOARD_PAN_Y=${BOARD_PAN_Y:--90}

if [[ ! -x "$APP" ]]; then
  echo "Screeps Arena executable not found: $APP" >&2
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

mkdir -p "$(dirname "$(realpath -m "$OUT")")"

TARGET=$(normalize_target "$TARGET")
TARGET=$(append_param "$TARGET" "replay-expanded=true")
TARGET=$(append_param "$TARGET" "board-capture=true")
TARGET=$(append_param "$TARGET" "board-zoom=$BOARD_ZOOM")
TARGET=$(append_param "$TARGET" "board-pan-x=$BOARD_PAN_X")
TARGET=$(append_param "$TARGET" "board-pan-y=$BOARD_PAN_Y")

DISPLAY_ID=${XVFB_DISPLAY:-$(find_display)}
XVFB_PID=
APP_PID=

cleanup() {
  if [[ -n "${APP_PID:-}" ]]; then
    kill "$APP_PID" >/dev/null 2>&1 || true
  fi
  pkill -f '/ScreepsArena/screeps_arena' >/dev/null 2>&1 || true
  if [[ -n "${XVFB_PID:-}" ]]; then
    kill "$XVFB_PID" >/dev/null 2>&1 || true
  fi
  rm -f /tmp/screeps-arena-board-capture
}
trap cleanup EXIT

touch /tmp/screeps-arena-board-capture
Xvfb "$DISPLAY_ID" -screen 0 "${WIDTH}x${HEIGHT}x24" -nolisten tcp >/tmp/screeps-arena-video-xvfb.log 2>&1 &
XVFB_PID=$!
sleep 1

DISPLAY="$DISPLAY_ID" \
  env -u WAYLAND_DISPLAY \
  XDG_SESSION_TYPE=x11 \
  SCREEPS_ARENA_DISABLE_DISCORD_RPC=1 \
  SCREEPS_ARENA_BOARD_CAPTURE=1 \
  SCREEPS_ARENA_BOARD_CAPTURE_ZOOM="$BOARD_ZOOM" \
  SCREEPS_ARENA_BOARD_CAPTURE_PAN_X="$BOARD_PAN_X" \
  SCREEPS_ARENA_BOARD_CAPTURE_PAN_Y="$BOARD_PAN_Y" \
  steam -applaunch 1137320 \
  --ozone-platform=x11 \
  --window-size="${WIDTH},${HEIGHT}" \
  --force-device-scale-factor=1 \
  "$TARGET" >/tmp/screeps-arena-video-app.log 2>&1 &
APP_PID=$!

sleep "$LOAD_WAIT"

ffmpeg -y \
  -f x11grab \
  -draw_mouse 0 \
  -framerate "$FPS" \
  -video_size "${WIDTH}x${HEIGHT}" \
  -i "${DISPLAY_ID}+0,0" \
  -t "$DURATION" \
  -an \
  -c:v libx264 \
  -preset veryfast \
  -crf 18 \
  -pix_fmt yuv420p \
  -movflags +faststart \
  "$OUT"
