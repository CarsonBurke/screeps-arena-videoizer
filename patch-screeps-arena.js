#!/usr/bin/env node
"use strict";

const fs = require("fs");
const path = require("path");

const appRoot = process.env.SCA_APP_ROOT ||
  "/home/marvin/.local/share/Steam/steamapps/common/ScreepsArena";
const dist = path.join(appRoot, "resources", "app", "dist");

const files = {
  index: path.join(dist, "index.js"),
  main: path.join(dist, "main.js"),
  styles: path.join(dist, "styles.css"),
};

for (const file of Object.values(files)) {
  if (!fs.existsSync(file)) {
    throw new Error(`Missing Screeps Arena bundle file: ${file}`);
  }
}

function backup(file) {
  const backupFile = `${file}.videoizer.bak`;
  if (!fs.existsSync(backupFile)) {
    fs.copyFileSync(file, backupFile);
  }
}

function read(file) {
  return fs.readFileSync(file, "utf8");
}

function write(file, content) {
  fs.writeFileSync(file, content);
}

function replaceOnce(content, from, to, label, done = to) {
  if (content.includes(done)) return content;
  if (!content.includes(from)) {
    throw new Error(`Could not patch ${label}; expected bundle text was not found.`);
  }
  return content.replace(from, to);
}

function replaceAll(content, from, to, label, done = to) {
  if (content.includes(done)) return content;
  if (!content.includes(from)) {
    throw new Error(`Could not patch ${label}; expected bundle text was not found.`);
  }
  return content.split(from).join(to);
}

function patchIndex() {
  const file = files.index;
  backup(file);
  let content = read(file);

  content = replaceOnce(
    content,
    'var path = __webpack_require__(/*! path */ "path");\nvar electron_1 = __webpack_require__(/*! electron */ "electron");',
    'var path = __webpack_require__(/*! path */ "path");\nvar fs = __webpack_require__(/*! fs */ "fs");\nvar electron_1 = __webpack_require__(/*! electron */ "electron");',
    "index fs import",
  );

  content = replaceOnce(
    content,
    '    console.log(123.1, scaleFactor, width, height, other);\n    var zoomFactor = 1;',
    "    console.log(123.1, scaleFactor, width, height, other);\n    var boardCaptureWindow = !!process.env.SCREEPS_ARENA_BOARD_CAPTURE || process.argv.some(function (arg) { return String(arg).indexOf('board-capture=true') !== -1; }) || fs.existsSync('/tmp/screeps-arena-board-capture');\n    var zoomFactor = 1;",
    "index board-capture flag",
  );

  content = replaceOnce(
    content,
    "        height: height,\n        minWidth: minWidth,",
    "        height: height,\n        x: 0,\n        y: 0,\n        minWidth: minWidth,",
    "index window position",
  );

  content = replaceOnce(
    content,
    "        minHeight: minHeight,\n        backgroundColor: '#191B21',",
    "        minHeight: minHeight,\n        fullscreen: boardCaptureWindow,\n        backgroundColor: '#191B21',",
    "index fullscreen option",
  );

  content = replaceOnce(
    content,
    "    });\n    (0, main_1.enable)(mainWindow);",
    "    });\n    if (boardCaptureWindow) {\n        mainWindow.setBounds({ x: 0, y: 0, width: width, height: height });\n        mainWindow.setContentSize(width, height);\n        mainWindow.setFullScreen(true);\n    }\n    (0, main_1.enable)(mainWindow);",
    "index fullscreen bounds",
    "mainWindow.setFullScreen(true);",
  );

  content = replaceOnce(
    content,
    "            devTools: constants_1.DEV_TOOLS,",
    "            devTools: constants_1.DEV_TOOLS || !!process.env.SCREEPS_ARENA_ENABLE_DEVTOOLS,",
    "index devtools flag",
  );

  content = replaceOnce(
    content,
    "    (0, ipc_discord_1.default)(mainWindow);\n    var hash = '';",
    "    if (!process.env.SCREEPS_ARENA_DISABLE_DISCORD_RPC) {\n        (0, ipc_discord_1.default)(mainWindow);\n    }\n    var hash = '';",
    "index Discord RPC flag",
  );

  write(file, content);
}

function patchMain() {
  const file = files.main;
  backup(file);
  let content = read(file);

  content = replaceOnce(
    content,
    "    this.zoomLevel = 0.06;\n    this.cell$ = new rxjs__WEBPACK_IMPORTED_MODULE_35__.BehaviorSubject(null);",
    `    const boardCaptureEnv = typeof process !== 'undefined' && process.env ? process.env : null;
    const boardCaptureParams = typeof URLSearchParams !== 'undefined' ? new URLSearchParams() : null;

    if (typeof location !== 'undefined' && boardCaptureParams) {
      [location.search, location.hash.split('?')[1]].forEach(part => {
        if (!part) {
          return;
        }

        new URLSearchParams(part).forEach((value, key) => boardCaptureParams.set(key, value));
      });
    }

    this.boardCapture = boardCaptureParams?.get('board-capture') === 'true' || !!boardCaptureEnv?.SCREEPS_ARENA_BOARD_CAPTURE;
    this.zoomLevel = this.boardCapture ? Number(boardCaptureParams?.get('board-zoom') || boardCaptureEnv?.SCREEPS_ARENA_BOARD_CAPTURE_ZOOM || 0.168) : 0.06;
    this.cell$ = new rxjs__WEBPACK_IMPORTED_MODULE_35__.BehaviorSubject(null);`,
    "main replay board-capture constructor",
    "this.boardCapture = boardCaptureParams?.get('board-capture') === 'true'",
  );

  content = replaceOnce(
    content,
    "    })); // Noop.\n  }\n",
    `    })); // Noop.

    if (this.boardCapture && typeof document !== 'undefined') {
      document.documentElement.classList.add('screeps-board-capture');
      document.body?.classList.add('screeps-board-capture');
      this.consoleAvailable = false;
    }
  }
`,
    "main replay capture CSS class",
    "document.documentElement.classList.add('screeps-board-capture');",
  );

  content = replaceAll(
    content,
    `      const {
        width,
        height
      } = this.elementRef.nativeElement.getBoundingClientRect();`,
    `      let {
        width,
        height
      } = this.elementRef.nativeElement.getBoundingClientRect();
      if (typeof document !== 'undefined' && document.documentElement.classList.contains('screeps-board-capture')) {
        width = window.innerWidth;
        height = window.innerHeight;
      }`,
    "main renderer viewport",
  );

  content = replaceAll(
    content,
    `        const {
          width,
          height
        } = _this2.elementRef.nativeElement.getBoundingClientRect();`,
    `        let {
          width,
          height
        } = _this2.elementRef.nativeElement.getBoundingClientRect();
        if (typeof document !== 'undefined' && document.documentElement.classList.contains('screeps-board-capture')) {
          width = window.innerWidth;
          height = window.innerHeight;
        }`,
    "main renderer init viewport",
  );

  content = replaceOnce(
    content,
    "      this.moveTo(hw - hs, hh - hs);\n",
    `      const boardCaptureParams = typeof URLSearchParams !== 'undefined' ? new URLSearchParams() : null;
      const boardCaptureEnv = typeof process !== 'undefined' && process.env ? process.env : null;
      if (typeof location !== 'undefined' && boardCaptureParams) {
        [location.search, location.hash.split('?')[1]].forEach(part => {
          if (!part) {
            return;
          }

          new URLSearchParams(part).forEach((value, key) => boardCaptureParams.set(key, value));
        });
      }
      const boardCapture = typeof document !== 'undefined' && document.documentElement.classList.contains('screeps-board-capture');
      const boardCapturePanX = boardCapture ? Number(boardCaptureParams?.get('board-pan-x') || boardCaptureEnv?.SCREEPS_ARENA_BOARD_CAPTURE_PAN_X || 0) : 0;
      const boardCapturePanY = boardCapture ? Number(boardCaptureParams?.get('board-pan-y') || boardCaptureEnv?.SCREEPS_ARENA_BOARD_CAPTURE_PAN_Y || 0) : 0;
      this.moveTo(hw - hs + boardCapturePanX, hh - hs + boardCapturePanY);
`,
    "main renderer board pan",
  );

  write(file, content);
}

function patchStyles() {
  const file = files.styles;
  backup(file);
  let content = read(file);
  if (content.includes("html.screeps-board-capture")) return;

  const css = `@charset "UTF-8";
html.screeps-board-capture,
html.screeps-board-capture body {
  --header-titlebar-height: 0px !important;
  overflow: hidden !important;
  width: 100vw !important;
  height: 100vh !important;
  background: #191B21 !important;
}

html.screeps-board-capture::after {
  display: none !important;
}

html.screeps-board-capture sca-root,
html.screeps-board-capture sca-arena,
html.screeps-board-capture app-arena,
html.screeps-board-capture router-outlet + * {
  position: fixed !important;
  inset: 0 !important;
  display: block !important;
  overflow: hidden !important;
  width: 100vw !important;
  height: 100vh !important;
  min-width: 0 !important;
  max-width: 100vw !important;
  min-height: 100vh !important;
  max-height: 100vh !important;
  margin: 0 !important;
  padding: 0 !important;
  zoom: 1 !important;
}

html.screeps-board-capture app-game-replay,
html.screeps-board-capture app-game-replay .__renderer,
html.screeps-board-capture app-game-replay screeps-renderer-actions,
html.screeps-board-capture app-game-replay screeps-renderer,
html.screeps-board-capture app-game-replay screeps-renderer-visual {
  position: fixed !important;
  inset: 0 !important;
  display: block !important;
  overflow: hidden !important;
  width: 100vw !important;
  height: 100vh !important;
  min-width: 100vw !important;
  min-height: 100vh !important;
  max-width: 100vw !important;
  max-height: 100vh !important;
  margin: 0 !important;
  padding: 0 !important;
}

html.screeps-board-capture app-game-replay header,
html.screeps-board-capture app-game-replay sca-replay-controls,
html.screeps-board-capture app-game-replay screeps-renderer-objects,
html.screeps-board-capture app-game-replay ui-scrollbar,
html.screeps-board-capture app-game-replay ui-resizable,
html.screeps-board-capture app-game-replay sca-replay-console,
html.screeps-board-capture app-game-replay app-game-cell,
html.screeps-board-capture app-game-replay sca-game-share,
html.screeps-board-capture app-game-replay sca-game-expand,
html.screeps-board-capture body > header,
html.screeps-board-capture #header,
html.screeps-board-capture sca-header,
html.screeps-board-capture sca-header-back,
html.screeps-board-capture sca-arena-background,
html.screeps-board-capture .sidebar {
  display: none !important;
}

html.screeps-board-capture canvas {
  cursor: none !important;
}
`;

  if (!content.startsWith('@charset "UTF-8";')) {
    throw new Error("Could not patch styles; expected charset header was not found.");
  }
  content = content.replace('@charset "UTF-8";', css);
  write(file, content);
}

patchIndex();
patchMain();
patchStyles();

console.log(`Patched Screeps Arena at ${appRoot}`);
