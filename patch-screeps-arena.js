#!/usr/bin/env node
"use strict";

const fs = require("fs");
const path = require("path");

const appCandidates = [
  process.env.SCA_APP_ROOT,
  path.join(
    process.env.HOME || "",
    ".local/share/Steam/steamapps/common/ScreepsArena",
  ),
  path.join(
    process.env.HOME || "",
    ".var/app/com.valvesoftware.Steam/.local/share/Steam/steamapps/common/ScreepsArena",
  ),
].filter(Boolean);
const appRoot = appCandidates.find((candidate) =>
  fs.existsSync(path.join(candidate, "resources", "app", "dist")),
) || appCandidates[0];
const dist = path.join(appRoot, "resources", "app", "dist");
const runtimeDir = path.join(dist, "screeps-arena-videoizer");
const projectDir = __dirname;
const runtimeNames = [
  "board-framing.js",
  "capture-config.js",
  "capture-rng.js",
  "capture-scene.js",
  "capture-transport.js",
  "capture-board-runtime.js",
  "renderer-actions.js",
  "renderer-calculations.js",
  "renderer-expressions.js",
  "renderer-processors.js",
  "renderer-random.js",
  "replay-batches.js",
  "replay-ir.js",
  "virtual-timeline.js",
];

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

function prepareBase(file, marker) {
  const backupFile = `${file}.videoizer.bak`;
  const content = read(file);
  if (!content.includes(marker)) {
    // A clean current bundle is authoritative. Refresh an older backup after a
    // Steam update instead of later rolling the installation back to that
    // obsolete version.
    fs.copyFileSync(file, backupFile);
    return content;
  }
  if (!fs.existsSync(backupFile)) {
    throw new Error(`Patched bundle has no pristine backup: ${file}`);
  }
  return read(backupFile);
}

function read(file) {
  return fs.readFileSync(file, "utf8");
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
  let content = prepareBase(file, "boardCaptureWindow");

  content = replaceOnce(
    content,
    '    console.log(123.1, scaleFactor, width, height, other);\n    var zoomFactor = 1;',
    "    console.log(123.1, scaleFactor, width, height, other);\n    var boardCaptureWindow = !!process.env.SCREEPS_ARENA_BOARD_CAPTURE || process.argv.some(function (arg) { return String(arg).indexOf('board-capture=true') !== -1; });\n    var zoomFactor = 1;",
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
    "        minHeight: minHeight,\n        // fullscreen: true,\n        backgroundColor: '#191B21',",
    "        minHeight: minHeight,\n        // fullscreen: true,\n        fullscreen: boardCaptureWindow,\n        backgroundColor: '#191B21',",
    "index fullscreen option",
  );

  content = replaceOnce(
    content,
    "    });\n    (0, main_1.enable)(mainWindow.webContents);",
    "    });\n    if (boardCaptureWindow) {\n        mainWindow.setBounds({ x: 0, y: 0, width: width, height: height });\n        mainWindow.setContentSize(width, height);\n        mainWindow.setFullScreen(true);\n    }\n    (0, main_1.enable)(mainWindow.webContents);",
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

  return { file, content };
}

function patchMain() {
  const file = files.main;
  let content = prepareBase(file, "screeps-board-capture");

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
    if (this.boardCapture && typeof globalThis !== 'undefined') {
      // Seed before Angular creates the child renderer. Metadata actions use
      // Math.random during scene construction; seeding later would leave the
      // initial scene nondeterministic even if subsequent replay objects were
      // deterministic.
      const boardCaptureRandomSeed = String(
        boardCaptureParams?.get('capture-random-seed')
          || boardCaptureEnv?.SCREEPS_ARENA_BOARD_CAPTURE_RANDOM_SEED
          || (typeof location !== 'undefined' ? location.hash.split('?')[0] : '')
          || 'screeps-arena-videoizer'
      );
      let boardCaptureSeedHash = 2166136261;
      for (const character of boardCaptureRandomSeed) {
        boardCaptureSeedHash ^= character.codePointAt(0);
        boardCaptureSeedHash = Math.imul(boardCaptureSeedHash, 16777619);
      }
      globalThis.__screepsArenaVideoizerRandomState = boardCaptureSeedHash >>> 0;
      if (!globalThis.__screepsArenaVideoizerOriginalRandom) {
        globalThis.__screepsArenaVideoizerOriginalRandom = Math.random;
      }
      globalThis.__screepsArenaVideoizerRandomSeed = boardCaptureRandomSeed;
      Math.random = () => {
        const boardCaptureRandomState = (
          globalThis.__screepsArenaVideoizerRandomState + 0x6D2B79F5
        ) | 0;
        globalThis.__screepsArenaVideoizerRandomState = boardCaptureRandomState;
        let value = Math.imul(
          boardCaptureRandomState ^ boardCaptureRandomState >>> 15,
          1 | boardCaptureRandomState
        );
        value ^= value + Math.imul(value ^ value >>> 7, 61 | value);
        return ((value ^ value >>> 14) >>> 0) / 4294967296;
      };
    }
    // Tag the document so the viewport/resize/centering overrides apply. This
    // must run in *this* (replay) component's constructor — before the child
    // renderer initializes — so the class is present when it reads the capture
    // size. (A generic '// Noop.' anchor matched the wrong component.)
    if (this.boardCapture && typeof document !== 'undefined') {
      document.documentElement.classList.add('screeps-board-capture');
      document.body?.classList.add('screeps-board-capture');
    }
    const boardCaptureZoom = boardCaptureParams?.get('board-zoom') || boardCaptureEnv?.SCREEPS_ARENA_BOARD_CAPTURE_ZOOM || 'auto';
    const boardCaptureNumericZoom = Number(boardCaptureZoom);
    if (this.boardCapture) {
      // Terrain is rasterized into this intermediate texture before the stage
      // is sampled at capture resolution. Keep it at least as detailed as the
      // projected board (within a conservative WebGL texture limit) to avoid
      // magnifying the old low-resolution terrain surface.
      const boardCaptureWidth = Number(boardCaptureParams?.get('capture-width') || boardCaptureEnv?.SCREEPS_ARENA_BOARD_CAPTURE_WIDTH) || 2048;
      const boardCaptureHeight = Number(boardCaptureParams?.get('capture-height') || boardCaptureEnv?.SCREEPS_ARENA_BOARD_CAPTURE_HEIGHT) || 2048;
      const boardCapturePadding = Number(boardCaptureParams?.get('board-padding') || boardCaptureEnv?.SCREEPS_ARENA_BOARD_CAPTURE_PADDING) || 0;
      const projectedBoardSize = Number.isFinite(boardCaptureNumericZoom) && boardCaptureNumericZoom > 0
        ? Math.ceil(this.worldConfig.VIEW_BOX * boardCaptureNumericZoom)
        : Math.max(1, Math.min(boardCaptureWidth, boardCaptureHeight) - 2 * boardCapturePadding);
      const requestedTextureSize = Number(boardCaptureParams?.get('capture-texture-size') || boardCaptureEnv?.SCREEPS_ARENA_BOARD_CAPTURE_TEXTURE_SIZE);
      const boardCaptureTextureSize = Number.isFinite(requestedTextureSize) && requestedTextureSize > 0
        ? Math.floor(requestedTextureSize)
        : Math.min(4096, Math.max(2048, Math.ceil(projectedBoardSize)));
      this.worldConfig.RENDER_SIZE = { width: boardCaptureTextureSize, height: boardCaptureTextureSize };
    }
    // "auto" framing needs the mounted renderer's true viewport and world
    // dimensions. Seed child initialization with a harmless finite value; the
    // capture runtime installs the exact auto-fit transform before frame zero.
    this.zoomLevel = this.boardCapture && Number.isFinite(boardCaptureNumericZoom) && boardCaptureNumericZoom > 0
      ? boardCaptureNumericZoom
      : 0.06;
    this.cell$ = new rxjs__WEBPACK_IMPORTED_MODULE_35__.BehaviorSubject(null);`,
    "main replay board-capture constructor",
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
        // Fix the logical viewport to the requested capture size so the board
        // framing (camera centering + zoom) is independent of the on-screen
        // window. A tiling/Wayland WM can give Electron an arbitrary window
        // size; the PIXI drawing buffer is independent of it, so we render the
        // full board at full resolution regardless. Fall back to the window
        // size when no explicit capture size is set.
        var __capEnv = (typeof process !== 'undefined' && process.env) ? process.env : {};
        var __capParams = new URLSearchParams((location.hash.split('?')[1] || location.search || ''));
        var __capW = Number(__capParams.get('capture-width') || __capEnv.SCREEPS_ARENA_BOARD_CAPTURE_WIDTH) || 0;
        var __capH = Number(__capParams.get('capture-height') || __capEnv.SCREEPS_ARENA_BOARD_CAPTURE_HEIGHT) || 0;
        width = __capW > 0 ? __capW : window.innerWidth;
        height = __capH > 0 ? __capH : window.innerHeight;
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
          // Fixed capture viewport, independent of the WM window size (see the
          // matching note on the other viewport site).
          var __capEnv = (typeof process !== 'undefined' && process.env) ? process.env : {};
          var __capParams = new URLSearchParams((location.hash.split('?')[1] || location.search || ''));
          var __capW = Number(__capParams.get('capture-width') || __capEnv.SCREEPS_ARENA_BOARD_CAPTURE_WIDTH) || 0;
          var __capH = Number(__capParams.get('capture-height') || __capEnv.SCREEPS_ARENA_BOARD_CAPTURE_HEIGHT) || 0;
          width = __capW > 0 ? __capW : window.innerWidth;
          height = __capH > 0 ? __capH : window.innerHeight;
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

  content = replaceOnce(
    content,
    `    const {
      width,
      height
    } = this.elementRef.nativeElement.getBoundingClientRect();

    this._gameApp.resize({
      width,
      height
    });`,
    `    let {
      width,
      height
    } = this.elementRef.nativeElement.getBoundingClientRect();
    if (typeof document !== 'undefined' && document.documentElement.classList.contains('screeps-board-capture')) {
      // Keep the renderer drawing buffer at the fixed capture size. This resize()
      // runs at init (and on window changes); without the override it clobbers
      // the buffer back to the WM-assigned window size, breaking framing/res.
      var __capEnv = (typeof process !== 'undefined' && process.env) ? process.env : {};
      var __capParams = new URLSearchParams((location.hash.split('?')[1] || location.search || ''));
      var __capW = Number(__capParams.get('capture-width') || __capEnv.SCREEPS_ARENA_BOARD_CAPTURE_WIDTH) || 0;
      var __capH = Number(__capParams.get('capture-height') || __capEnv.SCREEPS_ARENA_BOARD_CAPTURE_HEIGHT) || 0;
      width = __capW > 0 ? __capW : window.innerWidth;
      height = __capH > 0 ? __capH : window.innerHeight;
    }

    this._gameApp.resize({
      width,
      height
    });`,
    "main renderer resize",
    "Keep the renderer drawing buffer at the fixed capture size",
  );

  // Expose the per-player object mapper built inside the replay state pipeline so
  // the deterministic capture driver can replicate the exact remapping (object
  // _id/user -> player hash, plus the users map) when it applies state directly,
  // instead of routing through the tick$ -> getTick -> _state$ pipeline.
  content = replaceOnce(
    content,
    `        }

        return _this.tick$.pipe((0,rxjs_operators__WEBPACK_IMPORTED_MODULE_37__.takeUntil)(_this._destroySbj), (0,rxjs_operators__WEBPACK_IMPORTED_MODULE_42__.switchMap)(tick => {`,
    `        }

        _this._boardCaptureMapper = { players: players, users: users, mapStateObject: mapStateObject };

        return _this.tick$.pipe((0,rxjs_operators__WEBPACK_IMPORTED_MODULE_37__.takeUntil)(_this._destroySbj), (0,rxjs_operators__WEBPACK_IMPORTED_MODULE_42__.switchMap)(tick => {`,
    "main replay state mapper export",
    "_this._boardCaptureMapper =",
  );

  content = replaceOnce(
    content,
    "  _autoplay() {\n    this.screepsRendererRef.mounted$.pipe(",
    `  _captureBoard() {
    var self = this;
    var rendererRef = this.screepsRendererRef;
    if (!rendererRef) { return; }
    var mountedSub = rendererRef.mounted$.subscribe(function (mounted) {
      if (!mounted) { return; }
      if (mountedSub) { mountedSub.unsubscribe(); }
      else { setTimeout(function () { mountedSub?.unsubscribe(); }, 0); }
      var started = false;
      var readyTimer = null;
      var ticksSub = null;
      var startWhenReady = function () {
        var replayTicks = self._scaReplayStateService?._game?.game?.meta?.ticks;
        var ticksValue = self.ticks$.getValue();
        var metadataReady = Number.isSafeInteger(replayTicks) && replayTicks >= 0;
        // Prefer authoritative meta.ticks, but wait until ticks$ agrees so
        // capture never starts with a stale zero length while meta is ready.
        // Also wait past the default double-zero (meta and ticks both still 0)
        // until the replay service has a game object, so empty placeholders do
        // not encode a one-frame "success".
        if (started) { return; }
        var hasGame = !!self._scaReplayStateService?._game;
        if (metadataReady) {
          if (ticksValue !== replayTicks) { return; }
          if (replayTicks === 0 && !hasGame) { return; }
        } else if (!(Number.isSafeInteger(ticksValue) && ticksValue >= 0) || ticksValue < 1) {
          return;
        }
        started = true;
        if (readyTimer) { clearInterval(readyTimer); }
        if (ticksSub) { ticksSub.unsubscribe(); }
        self._runBoardCapture(metadataReady ? replayTicks : ticksValue);
      };
      // BehaviorSubject emits synchronously during subscribe(), before the
      // subscription variable is assigned. Defer the readiness check so cleanup
      // can always unsubscribe safely.
      ticksSub = self.ticks$.subscribe(function () { setTimeout(startWhenReady, 0); });
      readyTimer = setInterval(startWhenReady, 25);
      startWhenReady();
    });
  }

  _runBoardCapture(totalTicks) {
    // screeps-arena-videoizer-runtime-v2: keep the generated bundle patch tiny;
    // the implementation is copied next to it by this patcher and can be tested.
    var runtimePath = require('path').join(
      process.resourcesPath,
      'app',
      'dist',
      'screeps-arena-videoizer',
      'capture-board-runtime.js'
    );
    var options = Number.isSafeInteger(totalTicks) && totalTicks >= 0
      ? { totalTicks: totalTicks }
      : {};
    var captureParams = new URLSearchParams(
      location.hash.split('?')[1] || location.search || ''
    );
    options.closeWindow = captureParams.get('capture-close-window') === '1';
    require(runtimePath).captureBoard(this, options);
  }

  _autoplay() {
    if (this.boardCapture) {
      this._captureBoard();
      return;
    }
    this.screepsRendererRef.mounted$.pipe(`,
    "main board capture driver",
    "screeps-arena-videoizer-runtime-v2",
  );

  return { file, content };
}

function patchStyles() {
  const file = files.styles;
  let content = prepareBase(file, "html.screeps-board-capture");

  const css = `
/* screeps-arena-videoizer: board-capture styles (appended) */
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

  // Append (rather than splice after a charset header, which varies by build).
  // All board-capture rules are !important, so trailing position is fine.
  content = content + "\n" + css;
  return { file, content };
}

function runtimeSources() {
  return runtimeNames.map((name) => {
    const source = path.join(projectDir, name);
    if (!fs.existsSync(source)) throw new Error(`Missing runtime source: ${source}`);
    return { name, source };
  });
}

function installRuntime(sources) {
  fs.mkdirSync(runtimeDir, { recursive: true });
  const staged = [];
  const committed = [];
  try {
    for (const { name, source } of sources) {
      const destination = path.join(runtimeDir, name);
      const temporary = `${destination}.tmp-${process.pid}`;
      fs.copyFileSync(source, temporary);
      staged.push({
        temporary,
        destination,
        original: fs.existsSync(destination) ? fs.readFileSync(destination) : null,
      });
    }
    for (const entry of staged) {
      fs.renameSync(entry.temporary, entry.destination);
      committed.push(entry);
    }
  } catch (error) {
    for (const entry of committed.reverse()) {
      try {
        if (entry.original) fs.writeFileSync(entry.destination, entry.original);
        else fs.rmSync(entry.destination, { force: true });
      } catch (_) {}
    }
    throw error;
  } finally {
    for (const { temporary } of staged) fs.rmSync(temporary, { force: true });
  }
}

function commitBundles(outputs) {
  const staged = [];
  const committed = [];
  try {
    for (const { file, content } of outputs) {
      const temporary = `${file}.videoizer.tmp-${process.pid}`;
      fs.writeFileSync(temporary, content);
      staged.push({ file, temporary, original: fs.readFileSync(file) });
    }
    for (const entry of staged) {
      fs.renameSync(entry.temporary, entry.file);
      committed.push(entry);
    }
  } catch (error) {
    // A replacement-anchor or staging error happens before this function. If a
    // filesystem failure occurs during the short rename phase, restore every
    // bundle already replaced so the client never remains partially patched.
    for (const entry of committed.reverse()) {
      try { fs.writeFileSync(entry.file, entry.original); } catch (_) {}
    }
    throw error;
  } finally {
    for (const { temporary } of staged) fs.rmSync(temporary, { force: true });
  }
}

const sources = runtimeSources();
const outputs = [patchIndex(), patchMain(), patchStyles()];
installRuntime(sources);
commitBundles(outputs);

console.log(`Patched Screeps Arena at ${appRoot}`);
