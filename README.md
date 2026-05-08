# Screeps Arena Videoizer

Board-only video capture for Screeps Arena replays in the Steam/Electron client.

The recorder launches Screeps Arena through Steam inside a hidden Xvfb display,
opens a replay URL, hides the viewer UI, and records the game board surface to
H.264 with `ffmpeg`.

## Requirements

- Linux
- Steam with Screeps Arena installed
- `ffmpeg`
- `Xvfb`
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
  out/A1CWF344IR_board_1920.mp4 \
  20
```

The output defaults to `1920x1920`, 30 fps, CRF 18 H.264.

Useful overrides:

```bash
WIDTH=1920
HEIGHT=1920
FPS=30
LOAD_WAIT=90
BOARD_ZOOM=0.168
BOARD_PAN_X=-64
BOARD_PAN_Y=-90
```

The default board preset fits `A1CWF344IR` in a square frame without clipping.
For other arenas, tune `BOARD_ZOOM`, `BOARD_PAN_X`, and `BOARD_PAN_Y`.

## Notes

The script records the hidden Xvfb display directly with `x11grab`; it does not
capture the desktop or the visible Steam client. The temporary
`/tmp/screeps-arena-board-capture` sentinel is created only while recording so
the patched app can size its Electron window correctly for board capture.

