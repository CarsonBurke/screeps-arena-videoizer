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
    // Tag the document so the viewport/resize/centering overrides apply. This
    // must run in *this* (replay) component's constructor — before the child
    // renderer initializes — so the class is present when it reads the capture
    // size. (A generic '// Noop.' anchor matched the wrong component.)
    if (this.boardCapture && typeof document !== 'undefined') {
      document.documentElement.classList.add('screeps-board-capture');
      document.body?.classList.add('screeps-board-capture');
    }
    this.zoomLevel = this.boardCapture ? Number(boardCaptureParams?.get('board-zoom') || boardCaptureEnv?.SCREEPS_ARENA_BOARD_CAPTURE_ZOOM || 0.168) : 0.06;
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
        var __capW = Number(__capEnv.SCREEPS_ARENA_BOARD_CAPTURE_WIDTH) || 0;
        var __capH = Number(__capEnv.SCREEPS_ARENA_BOARD_CAPTURE_HEIGHT) || 0;
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
          var __capW = Number(__capEnv.SCREEPS_ARENA_BOARD_CAPTURE_WIDTH) || 0;
          var __capH = Number(__capEnv.SCREEPS_ARENA_BOARD_CAPTURE_HEIGHT) || 0;
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
      var __capW = Number(__capEnv.SCREEPS_ARENA_BOARD_CAPTURE_WIDTH) || 0;
      var __capH = Number(__capEnv.SCREEPS_ARENA_BOARD_CAPTURE_HEIGHT) || 0;
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
      mountedSub.unsubscribe();
      var start = function () { self._runBoardCapture(); };
      if (self.ticks$.getValue() >= 1) { start(); return; }
      var ticksSub = self.ticks$.subscribe(function (ticks) {
        if (ticks >= 1) { ticksSub.unsubscribe(); start(); }
      });
    });
  }

  _runBoardCapture() {
    var self = this;
    var fs = require('fs');
    var env = (typeof process !== 'undefined' && process.env) ? process.env : {};
    // The app renders inside Steam's pressure-vessel container, where the host
    // ffmpeg is not reachable. So we encode H.264 in-process (WebCodecs) and
    // stream the Annex-B elementary stream to a FIFO that the host opened (under
    // the output dir, which is bind-mounted into the container); the host just
    // remuxes it to MP4 with ffmpeg -c copy.
    var fifoPath = env.SCREEPS_ARENA_BOARD_CAPTURE_FIFO;
    if (!fifoPath) { return; }
    var metaPath = env.SCREEPS_ARENA_BOARD_CAPTURE_META || (fifoPath + '.meta');
    var fps = Number(env.SCREEPS_ARENA_BOARD_CAPTURE_FPS) || 30;
    var framesPerTick = Number(env.SCREEPS_ARENA_BOARD_CAPTURE_FRAMES_PER_TICK) || 8;
    var doneFile = env.SCREEPS_ARENA_BOARD_CAPTURE_DONE || '/tmp/screeps-arena-capture-done';
    var errFile = env.SCREEPS_ARENA_BOARD_CAPTURE_ERROR || '/tmp/screeps-arena-capture-error';
    var fail = function (msg) { try { fs.writeFileSync(errFile, String(msg)); } catch (e) {} };
    var dbgFile = env.SCREEPS_ARENA_BOARD_CAPTURE_DEBUG || '/tmp/sca-capture-debug.log';
    var dlog = function (msg) { try { fs.appendFileSync(dbgFile, '[' + Date.now() + '] ' + msg + '\\n'); } catch (e) {} };
    dlog('runBoardCapture: start');

    var gameApp = self.screepsRendererRef && self.screepsRendererRef._gameApp;
    if (!gameApp || !gameApp.app) { fail('board capture: renderer app unavailable'); return; }
    var app = gameApp.app;
    var renderer = app.renderer;
    var stage = app.stage;
    dlog('internals: getTick=' + (self._scaReplayStateService && typeof self._scaReplayStateService.getTick) + ' applyState=' + (typeof self.screepsRendererRef.applyState) + ' actionManager.update=' + (gameApp.actionManager && typeof gameApp.actionManager.update));
    var canvas = renderer.view || (renderer.context && renderer.context.canvas) || (renderer.gl && renderer.gl.canvas);
    if (typeof VideoEncoder === 'undefined' || typeof VideoFrame === 'undefined' || !canvas) {
      fail('board capture: WebCodecs (VideoEncoder/VideoFrame) or renderer canvas unavailable in this Electron');
      return;
    }
    var deltaSec = 1 / fps;
    var tickDurationSec = framesPerTick / fps;

    // Take deterministic control of the clock: no RAF loop, no Date.now-driven animate.
    try { app.ticker.autoStart = false; app.ticker.stop(); } catch (e) {}
    try { clearTimeout(gameApp.animateCheckerTimer); } catch (e) {}
    // Opaque background so extracted frames match the previous x11grab output.
    try { renderer.background.color = 0x191B21; renderer.background.alpha = 1; } catch (e) {}

    self.play = false;
    self._tickRate = tickDurationSec;

    var width = renderer.width;
    var height = renderer.height;
    var totalTicks = self.ticks$.getValue();
    if (!(totalTicks >= 1) || !(width >= 1) || !(height >= 1)) {
      fail('board capture: invalid geometry/ticks (' + width + 'x' + height + ', ticks=' + totalTicks + ')');
      return;
    }
    dlog('geometry: renderer ' + width + 'x' + height + ' screen=' + (renderer.screen && renderer.screen.width) + 'x' + (renderer.screen && renderer.screen.height) + ' res=' + renderer.resolution + ' envW=' + env.SCREEPS_ARENA_BOARD_CAPTURE_WIDTH + ' envH=' + env.SCREEPS_ARENA_BOARD_CAPTURE_HEIGHT + ' totalTicks=' + totalTicks);
    // Report the actual WebGL backend so we can tell hardware (NVIDIA/...) from
    // software (SwiftShader) rendering, which dominates capture throughput.
    try {
      var gl = renderer.gl || (renderer.context && renderer.context.gl);
      if (gl) {
        var __dbgInfo = gl.getExtension('WEBGL_debug_renderer_info');
        dlog('gl: type=' + renderer.type + ' RENDERER=' + gl.getParameter(gl.RENDERER) + ' VENDOR=' + gl.getParameter(gl.VENDOR) + (__dbgInfo ? ' UNMASKED=' + gl.getParameter(__dbgInfo.UNMASKED_RENDERER_WEBGL) : ''));
      } else { dlog('gl: context unavailable (renderer.type=' + renderer.type + ')'); }
    } catch (e) { dlog('gl: error ' + (e && e.message || e)); }

    // Each tick is rendered to the canvas and encoded in-process with WebCodecs:
    // VideoFrame(canvas) keeps the frame on the GPU (~free) and the H.264 encoder
    // emits an Annex-B elementary stream, so we never pay the gl.readPixels CPU
    // readback (~1-2s/frame) that bottlenecked the previous approach. The FIFO,
    // meta sidecar and encoder are set up in the capture loop below, after the
    // player mapper is ready and a supported encoder config has been chosen.

    // Fetch each tick's replay state directly and apply it, reproducing the
    // per-player object remapping the live RxJS pipeline does (object _id/user
    // rewritten to player hashes, plus the users map). Driving applyState
    // ourselves is deterministic and surfaces fetch errors directly, instead of
    // depending on the tick$ -> getTick -> _state$ pipeline whose catchError
    // silently swallows a slow/failed per-chunk fetch (which stalled capture).
    var stateService = self._scaReplayStateService;
    var applyTick = function (tick) {
      if (tick <= 3) dlog('applyTick ' + tick + ': getTick start');
      var fetched = Promise.resolve(stateService.getTick(tick));
      var timed = new Promise(function (_, reject) {
        setTimeout(function () { reject(new Error('board capture: getTick(' + tick + ') timed out after 20s')); }, 20000);
      });
      return Promise.race([fetched, timed]).then(function (state) {
        if (!state || !Array.isArray(state.objects)) {
          throw new Error('board capture: no state for tick ' + tick);
        }
        if (tick <= 3) dlog('applyTick ' + tick + ': getTick ok, objects=' + state.objects.length);
        var mapper = self._boardCaptureMapper;
        var mapped = mapper
          ? Object.assign({}, state, { objects: state.objects.map(mapper.mapStateObject), users: mapper.users })
          : state;
        self.screepsRendererRef.applyState(mapped, tickDurationSec);
        if (tick <= 3) dlog('applyTick ' + tick + ': applyState done');
      });
    };

    // The player hashes / object mapper are built by the replay state pipeline
    // once the players resolve (shortly after mount). Wait for it so the state
    // we apply directly matches what the renderer expects.
    var waitForMapper = function () {
      return new Promise(function (resolve, reject) {
        var waited = 0;
        var poll = function () {
          if (self._boardCaptureMapper) { resolve(); return; }
          waited += 50;
          if (waited > 30000) { reject(new Error('board capture: player object mapper not ready')); return; }
          setTimeout(poll, 50);
        };
        poll();
      });
    };

    (async function () {
      var stream = null;
      try {
        await waitForMapper();

        // Pick a supported H.264 config: prefer hardware, fall back to software.
        // realtime latency mode avoids B-frames so the Annex-B stream is in
        // display order and the host can mux it straight through (ffmpeg -c copy).
        var bitrate = Number(env.SCREEPS_ARENA_BOARD_CAPTURE_BITRATE) || 16000000;
        var keyint = Math.max(1, Math.round(fps * 2));
        var cands = [
          { codec: 'avc1.640033', hardwareAcceleration: 'prefer-hardware' },
          { codec: 'avc1.640033', hardwareAcceleration: 'no-preference' },
          { codec: 'avc1.42E033', hardwareAcceleration: 'no-preference' }
        ];
        var chosen = null;
        for (var ci = 0; ci < cands.length; ci++) {
          var cfg = { codec: cands[ci].codec, width: width, height: height, bitrate: bitrate, framerate: fps, latencyMode: 'realtime', avc: { format: 'annexb' }, hardwareAcceleration: cands[ci].hardwareAcceleration };
          var sup = null; try { sup = await VideoEncoder.isConfigSupported(cfg); } catch (e) {}
          if (sup && sup.supported) { chosen = cfg; break; }
        }
        if (!chosen) { throw new Error('no supported H.264 encoder config for ' + width + 'x' + height); }
        dlog('encoder: ' + chosen.codec + ' hw=' + chosen.hardwareAcceleration + ' bitrate=' + bitrate + ' keyint=' + keyint);

        // Announce geometry + fps, then open the FIFO (blocks in the threadpool
        // until the host ffmpeg reader attaches). The FIFO carries the H.264
        // Annex-B elementary stream the encoder emits, not raw frames.
        try { fs.writeFileSync(metaPath, width + ' ' + height + ' ' + fps + '\\n'); } catch (e) { throw new Error('cannot write meta: ' + e); }
        stream = fs.createWriteStream(fifoPath);
        var streamErr = null;
        stream.on('error', function (err) { streamErr = streamErr || err; });
        // Encoded chunks are tiny vs raw frames so the FIFO rarely backs up, but
        // honour write backpressure anyway to bound memory.
        var draining = null;
        var writeChunk = function (buf) {
          if (streamErr) { throw streamErr; }
          if (!stream.write(buf) && !draining) {
            draining = new Promise(function (res) { stream.once('drain', function () { draining = null; res(); }); });
          }
        };

        var encErr = null, encoded = 0;
        var encoder = new VideoEncoder({
          output: function (chunk) { encoded++; var b = new Uint8Array(chunk.byteLength); chunk.copyTo(b); try { writeChunk(Buffer.from(b)); } catch (e) { encErr = encErr || e; } },
          error: function (e) { encErr = encErr || e; }
        });
        encoder.configure(chosen);

        dlog('mapper ready; encoding ' + (totalTicks + 1) + ' ticks at ' + width + 'x' + height + ' (' + framesPerTick + ' frames/tick @' + fps + 'fps)');
        var globalFrame = 0;
        for (var tick = 0; tick <= totalTicks; tick++) {
          await applyTick(tick);
          var frames = tick === 0 ? 1 : framesPerTick;
          for (var f = 0; f < frames; f++) {
            gameApp.actionManager.update(deltaSec);
            renderer.render(stage);
            if (encErr) { throw encErr; }
            if (streamErr) { throw streamErr; }
            var vf = new VideoFrame(canvas, { timestamp: Math.round(globalFrame * 1e6 / fps), duration: Math.round(1e6 / fps) });
            encoder.encode(vf, { keyFrame: globalFrame % keyint === 0 });
            vf.close();
            globalFrame++;
            if (draining) { await draining; }
            while (encoder.encodeQueueSize > 8) {
              await new Promise(function (r) { setTimeout(r, 2); });
              if (encErr) { throw encErr; }
            }
          }
          if (tick % 100 === 0 || tick === totalTicks) { dlog('tick ' + tick + '/' + totalTicks + ' submitted=' + globalFrame + ' encoded=' + encoded); }
        }

        // Drain the encoder. Keep a heartbeat timer alive so an occluded window's
        // event loop is not throttled while we await the final flush.
        var hb = setInterval(function () {}, 100);
        try { await encoder.flush(); } finally { clearInterval(hb); }
        try { encoder.close(); } catch (e) {}
        if (encErr) { throw encErr; }
        if (streamErr) { throw streamErr; }
        dlog('encode complete; submitted=' + globalFrame + ' encoded=' + encoded + '; closing stream');
        // stream.end's callback fires once all bytes are flushed to the FIFO, so
        // the host ffmpeg has (or is reading) the full stream before we signal done.
        await new Promise(function (res) { stream.end(function () { res(); }); });
        fs.writeFileSync(doneFile, '');
        // Quit so the next run starts a fresh process: the host can't reliably
        // kill us when we run inside Steam's pressure-vessel PID namespace, and a
        // lingering instance would re-handle the next launch with stale state.
        try { if (typeof window !== 'undefined' && window.close) { window.close(); } } catch (e) {}
      } catch (err) {
        fail('board capture error: ' + (err && err.stack || err));
        try { if (stream) { stream.destroy(); } } catch (e) {}
      }
    })();
  }

  _autoplay() {
    if (this.boardCapture && typeof process !== 'undefined' && process.env && process.env.SCREEPS_ARENA_BOARD_CAPTURE_FIFO) {
      this._captureBoard();
      return;
    }
    this.screepsRendererRef.mounted$.pipe(`,
    "main board capture driver",
    "_runBoardCapture()",
  );

  write(file, content);
}

function patchStyles() {
  const file = files.styles;
  backup(file);
  let content = read(file);
  if (content.includes("html.screeps-board-capture")) return;

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
  write(file, content);
}

patchIndex();
patchMain();
patchStyles();

console.log(`Patched Screeps Arena at ${appRoot}`);
