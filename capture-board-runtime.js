"use strict";

const fs = require("fs");
const path = require("path");
const crypto = require("crypto");
const { applyBoardFrame, enforceBoardFrame } = require("./board-framing");
const {
  asPositiveInteger,
  configuredValue,
  resolveCaptureConfig,
} = require("./capture-config");
const {
  installSeededRendererRandom,
  preserveRendererRandomState,
  rendererRandomState,
  setRendererRandomState,
} = require("./capture-rng");
const {
  resetAnimatedSpritePhase,
  resetRendererScene,
  retainedTerrainObjects,
} = require("./capture-scene");
const {
  chooseEncoderConfig,
  createEncoderGate,
  createFifoFdWriter,
  openFifoForWrite,
  resolveCaptureTransport,
} = require("./capture-transport");
const rendererProcessors = require("./renderer-processors");
const replayBatches = require("./replay-batches");
const replayIR = require("./replay-ir");
const virtualTimeline = require("./virtual-timeline");

const DEFAULT_FIFO_OPEN_TIMEOUT_MS = 30_000;
const DEFAULT_MAPPER_TIMEOUT_MS = 30_000;
const DEFAULT_STATE_TIMEOUT_MS = 20_000;
const PIXI_LOW_UPDATE_PRIORITY = -50;

function nowMs() {
  if (typeof performance !== "undefined" && typeof performance.now === "function") {
    return performance.now();
  }
  return Number(process.hrtime.bigint()) / 1e6;
}

function errorText(error) {
  return String(error && (error.stack || error.message) || error);
}

function createLogger(debugFile) {
  if (!debugFile) {
    return function log() {
      // No durable debug sink configured; stay silent rather than writing shared /tmp.
    };
  }
  return function log(message, fields) {
    const suffix = fields === undefined ? "" : ` ${JSON.stringify(fields)}`;
    try {
      fs.appendFileSync(debugFile, `[${Date.now()}] ${message}${suffix}\n`);
    } catch (_) {
      // Logging must never mask the capture result.
    }
  };
}

function readCaptureParams(locationObject) {
  const params = new URLSearchParams();
  if (!locationObject || typeof URLSearchParams === "undefined") return params;
  for (const part of [locationObject.search, String(locationObject.hash || "").split("?")[1]]) {
    if (!part) continue;
    new URLSearchParams(part).forEach((value, key) => params.set(key, value));
  }
  return params;
}

function withTimeout(promise, timeoutMs, message) {
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(message)), timeoutMs);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

function resolveSharedTicker(app, options) {
  if (options.sharedTicker) return options.sharedTicker;
  const tickerConstructor = app.ticker && app.ticker.constructor;
  try {
    return tickerConstructor && tickerConstructor.shared || null;
  } catch (_) {
    return null;
  }
}

function takeClockControl(app, gameApp, options, log) {
  // Keep all Pixi-version-sensitive ticker handling in this adapter. Pixi 5-7
  // accept a numeric timestamp in update(); Pixi 8 preserves that public API
  // while delivering a Ticker object to listeners.
  const sharedTicker = resolveSharedTicker(app, options);
  const requested = options.tickers || [app.ticker, sharedTicker];
  const tickers = [...new Set(requested.filter(Boolean))];
  if (tickers.length === 0) {
    throw new Error("board capture: no Pixi ticker is available for virtual-time capture");
  }

  // Validate the complete set before mutating any ticker. Otherwise an invalid
  // secondary ticker could leave the application ticker stopped on failure.
  for (const ticker of tickers) {
    if (typeof ticker.stop !== "function" || typeof ticker.update !== "function") {
      throw new Error("board capture: unsupported Pixi ticker (missing stop/update)");
    }
  }

  const snapshots = tickers.map((ticker) => {
    const snapshot = {
      ticker,
      autoStart: ticker.autoStart,
      started: ticker.started,
      minFPS: ticker.minFPS,
      maxFPS: ticker.maxFPS,
      speed: ticker.speed,
      lastTime: ticker.lastTime,
    };
    ticker.autoStart = false;
    ticker.stop();
    ticker.maxFPS = 0;
    ticker.minFPS = 0;
    ticker.speed = 1;
    // Avoid a spurious first delta. All subsequent values are virtual DOMHighRes
    // timestamps relative to this origin.
    ticker.lastTime = 0;
    return snapshot;
  });

  let renderDetached = false;
  if (app.ticker && typeof app.ticker.remove === "function" && typeof app.render === "function") {
    app.ticker.remove(app.render, app);
    renderDetached = true;
  }
  let animateDetached = false;
  if (app.ticker && typeof app.ticker.remove === "function"
    && gameApp && typeof gameApp.animate === "function") {
    // actionManager is stepped explicitly below. Leaving GameApp.animate on the
    // ticker would step it a second time using its wall-clock delta.
    app.ticker.remove(gameApp.animate, gameApp);
    animateDetached = true;
  }

  log("clock-control", {
    tickers: tickers.length,
    appUsesSharedTicker: !!sharedTicker && app.ticker === sharedTicker,
    renderDetached,
    animateDetached,
  });

  return {
    tickers,
    restore() {
      if (renderDetached && app.ticker && typeof app.ticker.add === "function") {
        app.ticker.add(
          app.render,
          app,
          options.appRenderPriority ?? PIXI_LOW_UPDATE_PRIORITY,
        );
      }
      if (animateDetached && app.ticker && typeof app.ticker.add === "function") {
        app.ticker.add(
          gameApp.animate,
          gameApp,
          options.gameAnimatePriority ?? 0,
        );
      }
      for (const snapshot of snapshots) {
        const { ticker } = snapshot;
        try {
          ticker.stop();
          ticker.autoStart = snapshot.autoStart;
          // Pixi 7's minFPS setter clamps against maxFPS. Since maxFPS=0 means
          // "unlimited", restore a temporary positive ceiling first, then put
          // the original unlimited value back after minFPS is restored.
          if (snapshot.maxFPS === 0 && snapshot.minFPS > 0) {
            ticker.maxFPS = Math.max(60, snapshot.minFPS);
          } else {
            ticker.maxFPS = snapshot.maxFPS;
          }
          ticker.minFPS = snapshot.minFPS;
          ticker.maxFPS = snapshot.maxFPS;
          ticker.speed = snapshot.speed;
          ticker.lastTime = snapshot.lastTime;
          if (snapshot.started) ticker.start();
        } catch (_) {
          // Best effort only; the application normally closes after capture.
        }
      }
    },
  };
}

async function waitForMapper(component, timeoutMs) {
  if (component._boardCaptureMapper) return;
  let interval;
  try {
    await withTimeout(new Promise((resolve) => {
      interval = setInterval(() => {
        if (component._boardCaptureMapper) resolve();
      }, 25);
    }), timeoutMs, "board capture: player object mapper not ready");
  } finally {
    if (interval) clearInterval(interval);
  }
}

function writeTelemetry(file, telemetry) {
  try {
    fs.writeFileSync(file, `${JSON.stringify(telemetry, null, 2)}\n`);
  } catch (_) {
    // Telemetry is diagnostic and must not invalidate an otherwise good video.
  }
}

function resolveRendererVersion(options, env) {
  if (options.rendererVersion) return String(options.rendererVersion);
  if (env.SCREEPS_ARENA_RENDERER_VERSION) {
    return String(env.SCREEPS_ARENA_RENDERER_VERSION);
  }
  const resourcesPath = options.resourcesPath
    || (typeof process !== "undefined" && process.resourcesPath);
  if (!resourcesPath) return null;
  try {
    const packageMetadata = JSON.parse(fs.readFileSync(
      path.join(resourcesPath, "app", "package.json"),
      "utf8",
    ));
    return packageMetadata.dependencies
      && packageMetadata.dependencies["@screeps/renderer"]
      || null;
  } catch (_) {
    return null;
  }
}

function resolveRendererImplementationFingerprint(options) {
  if (options.rendererImplementationFingerprint) {
    return String(options.rendererImplementationFingerprint);
  }
  const resourcesPath = options.resourcesPath
    || (typeof process !== "undefined" && process.resourcesPath);
  if (!resourcesPath) return null;
  try {
    const bundlePath = path.join(resourcesPath, "app", "dist", "main.js");
    const originalBundlePath = `${bundlePath}.videoizer.bak`;
    return crypto.createHash("sha256").update(fs.readFileSync(
      fs.existsSync(originalBundlePath) ? originalBundlePath : bundlePath,
    )).digest("hex");
  } catch (_) {
    return null;
  }
}

const CONTENT_TYPE_BY_EXTENSION = Object.freeze({
  ".jpg": "image/jpeg",
  ".jpeg": "image/jpeg",
  ".png": "image/png",
  ".svg": "image/svg+xml",
  ".webp": "image/webp",
});

function bytesToDataUrl(bytes, contentType) {
  return `data:${contentType || "application/octet-stream"};base64,${Buffer.from(bytes).toString("base64")}`;
}

async function responseToDataUrl(response, label) {
  if (!response || response.ok === false || typeof response.arrayBuffer !== "function") {
    throw new Error(`board capture: failed to materialize ${label}`);
  }
  const bytes = Buffer.from(await response.arrayBuffer());
  const contentType = response.headers && typeof response.headers.get === "function"
    ? response.headers.get("content-type")
    : null;
  return bytesToDataUrl(bytes, contentType);
}

async function materializeExternalAssetUrls(value, fetchAsset, assetPath = "$", seen = new Map()) {
  if (typeof value === "string" && /^(?:blob:|https?:)/.test(value)) {
    if (typeof fetchAsset !== "function") {
      throw new Error(`board capture: cannot materialize ${assetPath} without fetch`);
    }
    return responseToDataUrl(await fetchAsset(value), assetPath);
  }
  if (!value || typeof value !== "object") return value;
  if (seen.has(value)) return seen.get(value);
  const result = Array.isArray(value) ? [] : {};
  seen.set(value, result);
  for (const key of Object.keys(value)) {
    result[key] = await materializeExternalAssetUrls(
      value[key],
      fetchAsset,
      `${assetPath}.${key}`,
      seen,
    );
  }
  return result;
}

// Retain the original helper name for callers that only materialize badge blob
// URLs; the implementation now also makes remote decoration assets portable.
const materializeBlobUrls = materializeExternalAssetUrls;

async function materializeRendererResources(resources, options = {}) {
  if (!resources || typeof resources !== "object" || Array.isArray(resources)) {
    throw new TypeError("renderer resources must be an object");
  }
  const fetchAsset = options.fetchAsset
    || (typeof globalThis !== "undefined" && typeof globalThis.fetch === "function"
      ? globalThis.fetch.bind(globalThis)
      : null);
  const resourcesPath = options.resourcesPath
    || (typeof process !== "undefined" && process.resourcesPath);
  const assetDirectory = resourcesPath && path.join(resourcesPath, "app", "dist");
  const result = {};
  await Promise.all(Object.keys(resources).sort().map(async (name) => {
    const value = resources[name];
    if (typeof value !== "string") {
      throw new TypeError(`renderer resource ${name} must be a string`);
    }
    if (value.startsWith("data:")) {
      result[name] = value;
      return;
    }

    if (/^(?:blob:|https?:)/.test(value)) {
      if (typeof fetchAsset !== "function") {
        throw new Error(`board capture: cannot materialize renderer resource ${name}`);
      }
      result[name] = await responseToDataUrl(await fetchAsset(value), `renderer resource ${name}`);
      return;
    }
    if (path.basename(value) !== value || !assetDirectory) {
      throw new Error(
        `board capture: renderer resource ${name} has unresolved path ${value}`,
      );
    }
    const bytes = fs.readFileSync(path.join(assetDirectory, value));
    const extension = path.extname(value).toLowerCase();
    result[name] = bytesToDataUrl(bytes, CONTENT_TYPE_BY_EXTENSION[extension]);
  }));
  return result;
}

async function loadVisualStates(visualService, totalTicks, concurrency, timeoutMs) {
  const ticks = Array.from({ length: totalTicks + 1 }, (_, tick) => tick);
  const loaded = await replayBatches.mapConcurrent(
    ticks,
    concurrency,
    async (tick) => {
      if (tick === 0 || tick === totalTicks) return [tick, []];
      const state = (await withTimeout(
        Promise.resolve(visualService.getTick(tick)),
        timeoutMs,
        `board capture: visual getTick(${tick}) timed out after ${timeoutMs}ms`,
      )) || [];
      return [tick, state];
    },
  );
  return new Map(loaded);
}

async function resolveReplayOwner(component, timeoutMs) {
  const source = component.isOwner$;
  if (!source) return false;
  if (typeof source.getValue === "function") return !!source.getValue();
  if (typeof source.subscribe !== "function") return false;

  let subscription;
  try {
    return !!await withTimeout(new Promise((resolve, reject) => {
      subscription = source.subscribe({ next: resolve, error: reject });
    }), timeoutMs, "board capture: timed out resolving replay ownership");
  } finally {
    if (subscription && typeof subscription.unsubscribe === "function") {
      subscription.unsubscribe();
    }
  }
}

function createVisualLayer(component, renderer, width, height, owner, log, preparedStates = null) {
  const visualRef = component.screepsRendererVisualRef;
  const visualService = component._scaReplayVisualService;
  const overlayCanvas = visualRef && visualRef.canvasRef && visualRef.canvasRef.nativeElement;
  const PIXI = typeof globalThis !== "undefined" && globalThis.PIXI;
  if (!owner || !visualService || !visualRef || !overlayCanvas || !PIXI) return null;

  if (overlayCanvas.width !== width) overlayCanvas.width = width;
  if (overlayCanvas.height !== height) overlayCanvas.height = height;
  const texture = PIXI.Texture.from(overlayCanvas);
  const sprite = new PIXI.Sprite(texture);
  const container = new PIXI.Container();
  container.addChild(sprite);
  log("visual-layer", { enabled: true, width: overlayCanvas.width, height: overlayCanvas.height });

  return {
    async applyTick(tick, totalTicks) {
      const objects = preparedStates
        ? preparedStates.get(tick)
        : tick > 0 && tick < totalTicks
          ? await visualService.getTick(tick)
          : [];
      visualRef.setVisual(objects || []);
      const source = texture.baseTexture || texture.source;
      if (source && typeof source.update === "function") source.update();
    },
    render() {
      renderer.render(container, { clear: false });
    },
    destroy() {
      try { container.destroy({ children: true, texture: true, baseTexture: false }); } catch (_) {}
    },
  };
}

function createTelemetry(config) {
  return {
    ok: false,
    startedAt: new Date().toISOString(),
    config: {
      width: config.width,
      height: config.height,
      fps: config.fps,
      framesPerTick: config.framesPerTick,
      ticksPerSecond: config.ticksPerSecond,
      simulationFps: config.simulationFps,
      fixedStepSeconds: config.fixedStepSeconds,
      bitrate: config.bitrate,
      totalTicks: config.totalTicks,
      encoderQueueLimit: config.encoderQueueLimit,
      boardZoom: config.boardZoom,
      boardPadding: config.boardPadding,
      boardPanX: config.boardPanX,
      boardPanY: config.boardPanY,
      compilerUnitTicks: config.compilerUnitTicks,
      preloadConcurrency: config.preloadConcurrency,
      compileReplayIR: config.compileReplayIR,
    },
    counts: {
      scheduled: 0,
      expected: null,
      submitted: 0,
      encoded: 0,
      rendered: 0,
      appliedTicks: 0,
      actionSubsteps: 0,
      encodedBytes: 0,
      visualTicks: 0,
    },
    peaks: { encoderQueue: 0, fifoPendingBytes: 0 },
    timingsMs: {
      mapperWait: 0,
      stateChunkPreload: 0,
      compilerPreparation: 0,
      replayIRAssetMaterialization: 0,
      replayIRCompilation: 0,
      replayIRWrite: 0,
      sceneReset: 0,
      visualChunkPreload: 0,
      visualStatePreparation: 0,
      scheduler: 0,
      encoderConfig: 0,
      fifoOpen: 0,
      stateFetch: 0,
      applyState: 0,
      visualUpdate: 0,
      actionUpdate: 0,
      tickerUpdate: 0,
      render: 0,
      videoFrame: 0,
      encodeSubmit: 0,
      encoderBackpressure: 0,
      fifoBackpressure: 0,
      encoderFlush: 0,
      fifoFinish: 0,
      total: 0,
    },
  };
}

/**
 * Deterministically captures a Screeps Arena replay component to Annex-B H.264.
 *
 * Timeline policy is delegated to virtual-timeline's runVirtualTimeline. Tests
 * and alternate schedulers may inject an API-compatible runner through
 * options.runVirtualTimeline.
 */
async function captureBoard(component, options = {}) {
  const env = options.env || (typeof process !== "undefined" ? process.env : {});
  const params = options.params || readCaptureParams(
    options.location || (typeof location !== "undefined" ? location : null),
  );
  const transport = resolveCaptureTransport(options, params, env);
  const {
    rawCaptureId,
    captureId,
    fifoPath,
    errorFile,
    doneFile,
    debugFile,
  } = transport;
  const log = options.log || createLogger(debugFile);
  const fail = (message) => {
    if (!errorFile) return;
    try { fs.writeFileSync(errorFile, String(message)); } catch (_) {}
  };

  let telemetry;
  let telemetryFile;
  let writer;
  let encoder;
  let encoderGate;
  let clockControl;
  let visualLayer;
  let stateSnapshot;
  let originalRandom;
  const captureStart = nowMs();

  try {
    if (rawCaptureId && !captureId) {
      throw new Error("board capture: invalid capture-id");
    }
    if (!component || !component.screepsRendererRef) {
      throw new Error("board capture: replay component/renderer reference is unavailable");
    }
    if (!fifoPath) throw new Error("board capture: FIFO path is not configured");
    if (typeof Buffer === "undefined") {
      throw new Error("board capture: Node Buffer is unavailable (nodeIntegration required)");
    }

    const gameApp = component.screepsRendererRef._gameApp;
    const app = gameApp && gameApp.app;
    const renderer = app && app.renderer;
    const stage = app && app.stage;
    const actionManager = gameApp && gameApp.actionManager;
    const stateService = component._scaReplayStateService;
    if (!app || !renderer || !stage) {
      throw new Error("board capture: Pixi application internals are unavailable");
    }
    if (!actionManager || typeof actionManager.update !== "function") {
      throw new Error("board capture: action manager update API is unavailable");
    }
    if (!stateService || typeof stateService.getTick !== "function") {
      throw new Error("board capture: replay state service is unavailable");
    }
    if (typeof gameApp.applyState !== "function") {
      throw new Error("board capture: game renderer applyState API is unavailable");
    }
    const applyRendererState = options.applyState || gameApp.applyState.bind(gameApp);

    const VideoEncoderClass = options.VideoEncoder
      || (typeof VideoEncoder !== "undefined" && VideoEncoder);
    const VideoFrameClass = options.VideoFrame
      || (typeof VideoFrame !== "undefined" && VideoFrame);
    const canvas = options.canvas
      || renderer.view
      || renderer.canvas
      || renderer.context && renderer.context.canvas
      || renderer.gl && renderer.gl.canvas;
    if (!VideoEncoderClass || !VideoFrameClass || !canvas) {
      throw new Error("board capture: WebCodecs or renderer canvas is unavailable");
    }

    const metaTicks = stateService._game
      && stateService._game.game
      && stateService._game.game.meta
      && stateService._game.game.meta.ticks;
    const ticksObservableValue = component.ticks$ && component.ticks$.getValue();
    // Prefer explicit options, then authoritative replay meta, then the
    // BehaviorSubject mirror. Meta alone used to race ahead of ticks$ and
    // produce zero-length captures.
    const totalTicksHint = Number.isSafeInteger(metaTicks) && metaTicks >= 0
      ? metaTicks
      : ticksObservableValue;
    const config = resolveCaptureConfig({
      options,
      params,
      env,
      canvasHints: {
        width: canvas.width || renderer.width,
        height: canvas.height || renderer.height,
      },
      totalTicksHint,
    });
    const {
      width,
      height,
      fps,
      framesPerTick,
      ticksPerSecond,
      simulationFps,
      fixedStepSeconds,
      bitrate,
      totalTicks,
      encoderQueueLimit,
      tickDurationSeconds,
      boardZoom,
      boardPadding,
      boardPanX,
      boardPanY,
      compilerUnitTicks,
      preloadConcurrency,
      compileReplayIR,
    } = config;
    telemetry = createTelemetry(config);
    const replaySeed = configuredValue(
      options,
      "randomSeed",
      params,
      "capture-random-seed",
      env,
      "SCREEPS_ARENA_BOARD_CAPTURE_RANDOM_SEED",
    )
      ?? (stateService._game && stateService._game._id)
      ?? "screeps-arena-videoizer";
    telemetry.randomSeed = String(replaySeed);
    const bootRandomSeed = typeof globalThis !== "undefined"
      ? globalThis.__screepsArenaVideoizerRandomSeed
      : null;
    originalRandom = typeof globalThis !== "undefined"
      && globalThis.__screepsArenaVideoizerOriginalRandom
      || Math.random;
    telemetry.randomSeededBeforeRenderer = String(bootRandomSeed) === String(replaySeed);
    if (!telemetry.randomSeededBeforeRenderer) {
      // Keep global PRNG state and Math.random on the same stream. Replacing
      // only Math.random previously left __screepsArenaVideoizerRandomState on
      // the boot seed, so later get/set helpers diverged from Math.random.
      installSeededRendererRandom(replaySeed);
    }
    telemetryFile = transport.telemetryFile || `${fifoPath}.telemetry.json`;

    log("capture-start", telemetry.config);
    if (canvas.width && canvas.height
      && (canvas.width !== width || canvas.height !== height)) {
      throw new Error(
        `board capture: encoder geometry ${width}x${height} does not match canvas ${canvas.width}x${canvas.height}`,
      );
    }

    stateSnapshot = {
      play: component.play,
      tickRate: component._tickRate,
      background: renderer.background ? {
        color: renderer.background.color,
        alpha: renderer.background.alpha,
      } : null,
      stage: {
        scaleX: stage.scale.x,
        scaleY: stage.scale.y,
        positionX: stage.position.x,
        positionY: stage.position.y,
      },
    };
    component.play = false;
    component._tickRate = tickDurationSeconds;
    try { clearTimeout(gameApp.animateCheckerTimer); } catch (_) {}
    try {
      renderer.background.color = 0x191B21;
      renderer.background.alpha = 1;
    } catch (_) {}

    clockControl = takeClockControl(app, gameApp, options, log);
    const boardFrame = applyBoardFrame(component, config);
    telemetry.boardFrame = boardFrame;
    log("board-frame", boardFrame);
    try {
      const gl = renderer.gl || renderer.context && renderer.context.gl;
      if (gl) {
        const debugInfo = gl.getExtension("WEBGL_debug_renderer_info");
        log("gpu", {
          rendererType: renderer.type,
          renderer: gl.getParameter(gl.RENDERER),
          vendor: gl.getParameter(gl.VENDOR),
          unmaskedRenderer: debugInfo
            ? gl.getParameter(debugInfo.UNMASKED_RENDERER_WEBGL)
            : null,
        });
      }
    } catch (error) {
      log("gpu-query-failed", { error: errorText(error) });
    }

    let mark = nowMs();
    if (options.requireMapper !== false) {
      await waitForMapper(
        component,
        asPositiveInteger(options.mapperTimeoutMs, DEFAULT_MAPPER_TIMEOUT_MS, "mapperTimeoutMs"),
      );
    }
    telemetry.timingsMs.mapperWait = nowMs() - mark;
    const stateTimeoutMs = asPositiveInteger(
      options.stateTimeoutMs,
      DEFAULT_STATE_TIMEOUT_MS,
      "stateTimeoutMs",
    );
    const mapper = component._boardCaptureMapper;
    let mappedUsers = mapper && mapper.users;
    if (compileReplayIR && mappedUsers) {
      const assetStart = nowMs();
      const fetchBlob = options.fetchBlob
        || (typeof globalThis !== "undefined" && typeof globalThis.fetch === "function"
          ? globalThis.fetch.bind(globalThis)
          : null);
      mappedUsers = await materializeExternalAssetUrls(mappedUsers, fetchBlob, "$.users");
      telemetry.timingsMs.replayIRAssetMaterialization = nowMs() - assetStart;
    }
    if (typeof stateService.getChunk === "function") {
      const preloadStart = nowMs();
      const chunks = await replayBatches.preloadReplayChunks({
        totalTicks,
        concurrency: preloadConcurrency,
        loadChunk: (endTick) => withTimeout(
          Promise.resolve(stateService.getChunk(endTick, undefined, stateService.chunks, false)),
          stateTimeoutMs,
          `board capture: replay chunk ${endTick} timed out after ${stateTimeoutMs}ms`,
        ),
      });
      telemetry.timingsMs.stateChunkPreload = nowMs() - preloadStart;
      telemetry.stateChunks = {
        count: chunks.size,
        concurrency: preloadConcurrency,
      };
    }
    const loadMappedState = async (tick) => {
      const fetchStart = nowMs();
      const state = await withTimeout(
        Promise.resolve(stateService.getTick(tick)),
        stateTimeoutMs,
        `board capture: getTick(${tick}) timed out after ${stateTimeoutMs}ms`,
      );
      telemetry.timingsMs.stateFetch += nowMs() - fetchStart;
      if (!state || !Array.isArray(state.objects)) {
        throw new Error(`board capture: no valid state for tick ${tick}`);
      }
      return mapper
        ? Object.assign({}, state, {
          objects: state.objects.map(mapper.mapStateObject),
          users: mappedUsers,
        })
        : state;
    };
    const batchPreparationStart = nowMs();
    const preparedReplay = await replayBatches.prepareStateBatches({
      totalTicks,
      ticksPerBatch: compilerUnitTicks,
      concurrency: preloadConcurrency,
      loadState: loadMappedState,
      // Pixi still consumes full sequential states. Delta compilation is a
      // backend boundary for the temporal renderer and would only add work here.
      compileDeltas: options.compileReplayDeltas === true,
    });
    telemetry.timingsMs.compilerPreparation = nowMs() - batchPreparationStart;
    telemetry.compilerUnits = {
      count: preparedReplay.plans.length,
      ticksPerUnit: compilerUnitTicks,
      preloadConcurrency,
      compiledDeltas: preparedReplay.batches.length > 0,
      pixiResumable: false,
      parallelRenderer: false,
    };

    const hasVisualPipeline = component.screepsRendererVisualRef
      && component._scaReplayVisualService;
    let preparedVisualStates = null;
    let visualOverlayEnabled = false;
    if (hasVisualPipeline) {
      const owner = await resolveReplayOwner(component, asPositiveInteger(
        options.ownerTimeoutMs,
        DEFAULT_MAPPER_TIMEOUT_MS,
        "ownerTimeoutMs",
      ));
      const visualService = component._scaReplayVisualService;
      if (owner && typeof visualService.getChunk === "function") {
        const preloadStart = nowMs();
        const chunks = await replayBatches.preloadReplayChunks({
          totalTicks,
          includeInitial: false,
          concurrency: preloadConcurrency,
          loadChunk: (endTick) => withTimeout(
            Promise.resolve(visualService.getChunk(endTick)),
            stateTimeoutMs,
            `board capture: visual chunk ${endTick} timed out after ${stateTimeoutMs}ms`,
          ),
        });
        telemetry.timingsMs.visualChunkPreload = nowMs() - preloadStart;
        telemetry.visualChunks = {
          count: chunks.size,
          concurrency: preloadConcurrency,
        };
      }
      visualOverlayEnabled = !!owner;
      if (compileReplayIR && owner && visualService && typeof visualService.getTick === "function") {
        const visualStart = nowMs();
        preparedVisualStates = await loadVisualStates(
          visualService,
          totalTicks,
          preloadConcurrency,
          stateTimeoutMs,
        );
        telemetry.timingsMs.visualStatePreparation = nowMs() - visualStart;
      }
      visualLayer = createVisualLayer(
        component,
        renderer,
        width,
        height,
        owner,
        log,
        preparedVisualStates,
      );
      visualOverlayEnabled = !!visualLayer;
    }

    const sceneResetStart = nowMs();
    telemetry.sceneReset = resetRendererScene(gameApp);
    telemetry.counts.resetAnimatedSprites = options.resetAnimationPhase === false
      ? 0
      : resetAnimatedSpritePhase(stage);
    telemetry.timingsMs.sceneReset = nowMs() - sceneResetStart;
    log("scene-reset", Object.assign({}, telemetry.sceneReset, {
      animatedSprites: telemetry.counts.resetAnimatedSprites,
    }));

    if (compileReplayIR) {
      const irStart = nowMs();
      const world = gameApp.world;
      const assetStart = nowMs();
      const fetchAsset = options.fetchBlob
        || (typeof globalThis !== "undefined" && typeof globalThis.fetch === "function"
          ? globalThis.fetch.bind(globalThis)
          : null);
      const [rendererResources, rendererDecorations] = await Promise.all([
        materializeRendererResources(
          world && world.resourceMap || {},
          {
            fetchAsset,
            resourcesPath: options.resourcesPath,
          },
        ),
        materializeExternalAssetUrls(
          world && world.decorations || [],
          fetchAsset,
          "$.decorations",
        ),
      ]);
      telemetry.timingsMs.replayIRAssetMaterialization += nowMs() - assetStart;
      const rendererContract = replayIR.compileRendererContract({
        rendererVersion: resolveRendererVersion(options, env),
        rendererImplementationFingerprint: resolveRendererImplementationFingerprint(options),
        metadata: world && world.metadata,
        resources: rendererResources,
        decorations: rendererDecorations,
        terrain: retainedTerrainObjects(world),
        worldOptions: world && world.options,
      });
      const rendererGraphEvaluator = new rendererProcessors.RendererGraphEvaluator({
        metadata: world && world.metadata,
        world,
        random: Math.random,
        getRandomState: rendererRandomState,
        setRandomState: setRendererRandomState,
      });
      const compiledReplay = preserveRendererRandomState(() => replayIR.compileReplayIR({
        states: preparedReplay.statesByTick,
        totalTicks,
        framesPerSecond: fps,
        ticksPerSecond,
        substepsPerSecond: 1 / fixedStepSeconds,
        tickTransitionSeconds: tickDurationSeconds,
        renderConfig: {
          width,
          height,
          backgroundColor: 0x191B21,
          boardFrame,
        },
        randomSeed: replaySeed,
        randomStateAtFirstTick: rendererRandomState(),
        visualOverlayEnabled,
        visualStates: preparedVisualStates,
        rendererTickEvaluator: (state, tick) => (
          rendererGraphEvaluator.evaluateTick(
            state,
            tick,
            tickDurationSeconds,
          )
        ),
        rendererContract,
      }));
      telemetry.timingsMs.replayIRCompilation = nowMs() - irStart;
      const replayIRFile = options.replayIRFile
        || (transport.transportPrefix && `${transport.transportPrefix}.replay-ir.json`);
      if (replayIRFile) {
        const writeStart = nowMs();
        const temporaryFile = `${replayIRFile}.partial-${process.pid}`;
        fs.writeFileSync(temporaryFile, `${JSON.stringify({
          rendererContract,
          replay: compiledReplay,
        })}\n`);
        fs.renameSync(temporaryFile, replayIRFile);
        telemetry.timingsMs.replayIRWrite = nowMs() - writeStart;
      }
      telemetry.replayIR = {
        file: replayIRFile || null,
        fingerprint: compiledReplay.fingerprint,
        rendererContractFingerprint: rendererContract.fingerprint,
        entities: compiledReplay.entities.length,
        actionEvents: compiledReplay.actionEvents.length,
        rendererEvents: compiledReplay.rendererGraph.columns[0].length,
        semantics: rendererContract.inventory,
      };
      if (typeof options.onReplayIR === "function") {
        await options.onReplayIR({ rendererContract, replay: compiledReplay });
      }
    }

    const timelineApi = options.timeline || virtualTimeline;
    const runVirtualTimeline = options.runVirtualTimeline || timelineApi.runVirtualTimeline;
    if (typeof runVirtualTimeline !== "function") {
      throw new Error("board capture: virtual timeline runner is unavailable");
    }
    const frameCount = asPositiveInteger(
      options.frameCount,
      timelineApi.calculateFrameCount({
        totalTicks,
        framesPerSecond: fps,
        ticksPerSecond,
      }),
      "frameCount",
    );
    const timelineOptions = Object.assign({}, options.timelineOptions, {
      frameCount,
      totalTicks,
      targetTick: totalTicks,
      framesPerSecond: fps,
      ticksPerSecond,
      substepsPerSecond: 1 / fixedStepSeconds,
    });
    const temporalWorkUnits = replayBatches.planTemporalWorkUnits({
      totalTicks,
      ticksPerUnit: compilerUnitTicks,
      framesPerSecond: fps,
      ticksPerSecond,
    });
    const workUnitFrames = temporalWorkUnits.reduce((sum, unit) => sum + unit.frameCount, 0);
    if (workUnitFrames !== frameCount) {
      throw new Error(`board capture: temporal work units expected ${frameCount}, got ${workUnitFrames}`);
    }
    telemetry.temporalWorkUnits = {
      count: temporalWorkUnits.length,
      ticksPerUnit: compilerUnitTicks,
      frames: workUnitFrames,
      pixiResumable: false,
      parallelRenderer: false,
      checkpointKind: "replay-data-only",
    };
    telemetry.counts.expected = frameCount;
    const hooks = options.timelineHooks || {};
    if (typeof hooks.beforeCapture === "function") {
      await hooks.beforeCapture({ component, gameApp, app, renderer, config, telemetry });
    }

    mark = nowMs();
    const encoderConfig = await chooseEncoderConfig(VideoEncoderClass, config);
    telemetry.timingsMs.encoderConfig = nowMs() - mark;
    telemetry.encoder = encoderConfig;
    const keyFrameInterval = asPositiveInteger(
      options.keyFrameInterval,
      Math.max(1, Math.round(fps * 2)),
      "keyFrameInterval",
    );
    log("encoder-config", encoderConfig);

    const metaPath = options.metaPath
      || transport.metaFile
      || `${fifoPath}.meta`;
    fs.writeFileSync(metaPath, `${width} ${height} ${fps}\n`);
    mark = nowMs();
    const fifoFd = await openFifoForWrite(
      fifoPath,
      asPositiveInteger(
        options.fifoOpenTimeoutMs,
        DEFAULT_FIFO_OPEN_TIMEOUT_MS,
        "fifoOpenTimeoutMs",
      ),
    );
    writer = createFifoFdWriter(fifoFd, fs, 1, () => {
      if (encoderGate) encoderGate.wake();
    });
    telemetry.timingsMs.fifoOpen = nowMs() - mark;

    let encoderError = null;
    encoder = new VideoEncoderClass({
      output(chunk) {
        try {
          const bytes = new Uint8Array(chunk.byteLength);
          chunk.copyTo(bytes);
          writer.enqueue(Buffer.from(bytes));
          telemetry.counts.encoded++;
          telemetry.counts.encodedBytes += bytes.byteLength;
          telemetry.peaks.fifoPendingBytes = Math.max(
            telemetry.peaks.fifoPendingBytes,
            writer.pendingBytes,
          );
        } catch (error) {
          encoderError = encoderError || error;
          if (encoderGate) encoderGate.wake();
        }
      },
      error(error) {
        encoderError = encoderError || error;
        if (encoderGate) encoderGate.wake();
      },
    });
    encoderGate = createEncoderGate(encoder, () => encoderError || writer.error);
    encoder.configure(encoderConfig);

    const renderFrame = options.renderFrame || (() => renderer.render(stage));
    let globalFrame = 0;
    let previousTimestampUs = -1;
    let lastAppliedTick = -1;
    let lastVirtualMs = 0;

    const timelineHooks = {
      async applyTick(event) {
        const tick = Number(event.tick);
        if (!Number.isSafeInteger(tick) || tick < 0 || tick > totalTicks) {
          throw new Error(`board capture: timeline emitted invalid tick ${event.tick}`);
        }
        if (tick !== lastAppliedTick + 1) {
          throw new Error(
            `board capture: tick invariant failed: expected ${lastAppliedTick + 1}, got ${tick}`,
          );
        }
        if (typeof hooks.beforeApplyTick === "function") {
          await hooks.beforeApplyTick({ event, tick, component, gameApp, config, telemetry });
        }
        const mapped = event.state || preparedReplay.statesByTick.get(tick);
        if (!mapped) throw new Error(`board capture: prepared state ${tick} is unavailable`);
        mark = nowMs();
        // Call the renderer core directly. The Angular wrapper also publishes
        // every state through UI observables and schedules a timer solely for
        // replay controls; both are invisible in board-only capture and were
        // the dominant measured cost.
        applyRendererState(mapped, tickDurationSeconds);
        telemetry.timingsMs.applyState += nowMs() - mark;
        if (visualLayer) {
          mark = nowMs();
          await withTimeout(
            visualLayer.applyTick(tick, totalTicks),
            stateTimeoutMs,
            `board capture: visual getTick(${tick}) timed out after ${stateTimeoutMs}ms`,
          );
          telemetry.timingsMs.visualUpdate += nowMs() - mark;
          telemetry.counts.visualTicks++;
        }
        telemetry.counts.appliedTicks++;
        lastAppliedTick = tick;
        if (typeof hooks.afterApplyTick === "function") {
          await hooks.afterApplyTick({ event, tick, component, gameApp, config, telemetry });
        }
        if (tick % 100 === 0 || tick === totalTicks) {
          log("capture-progress", {
            tick,
            totalTicks,
            submitted: telemetry.counts.submitted,
            encoded: telemetry.counts.encoded,
          });
        }
      },

      async advance(event) {
        const durationSeconds = Number(event.durationSeconds);
        if (!Number.isFinite(durationSeconds)
          || durationSeconds <= 0
          || durationSeconds > fixedStepSeconds * (1 + 1e-6)) {
          throw new Error(
            `board capture: invalid timeline substep ${event.durationSeconds}; maximum is ${fixedStepSeconds}`,
          );
        }
        if (typeof hooks.beforeAdvance === "function") {
          await hooks.beforeAdvance({ event, component, gameApp, config, telemetry });
        }
        mark = nowMs();
        actionManager.update(durationSeconds);
        telemetry.timingsMs.actionUpdate += nowMs() - mark;
        const virtualMs = event.to && typeof event.to.toNumber === "function"
          ? event.to.toNumber() * 1000
          : lastVirtualMs + durationSeconds * 1000;
        if (!Number.isFinite(virtualMs) || virtualMs <= lastVirtualMs) {
          throw new Error(`board capture: non-monotonic virtual time ${virtualMs}`);
        }
        mark = nowMs();
        for (const ticker of clockControl.tickers) ticker.update(virtualMs);
        telemetry.timingsMs.tickerUpdate += nowMs() - mark;
        telemetry.counts.actionSubsteps++;
        lastVirtualMs = virtualMs;
        if (typeof hooks.afterAdvance === "function") {
          await hooks.afterAdvance({ event, virtualMs, component, gameApp, config, telemetry });
        }
      },

      async render(event) {
        const frameNumber = Number(event.frame);
        const tick = Number(event.tick);
        if (!Number.isSafeInteger(frameNumber) || frameNumber !== globalFrame) {
          throw new Error(
            `board capture: frame invariant failed: expected ${globalFrame}, got ${event.frame}`,
          );
        }
        if (!Number.isSafeInteger(tick) || tick !== lastAppliedTick) {
          throw new Error(
            `board capture: render target-state invariant failed: applied tick ${lastAppliedTick}, render tick ${event.tick}`,
          );
        }
        if (typeof hooks.beforeRender === "function") {
          await hooks.beforeRender({ event, globalFrame, component, gameApp, config, telemetry });
        }
        mark = nowMs();
        enforceBoardFrame(stage, boardFrame);
        await renderFrame({ renderer, stage, app, event, globalFrame });
        if (visualLayer) visualLayer.render();
        telemetry.timingsMs.render += nowMs() - mark;
        telemetry.counts.rendered++;

        if (encoderError) throw encoderError;
        if (writer.error) throw writer.error;

        const timestampUs = Number(event.timestampUs);
        const durationUs = Number(event.durationUs);
        if (!Number.isSafeInteger(timestampUs) || timestampUs <= previousTimestampUs) {
          throw new Error(`board capture: invalid/non-monotonic timestamp ${event.timestampUs}`);
        }
        if (!Number.isSafeInteger(durationUs) || durationUs <= 0) {
          throw new Error(`board capture: invalid frame duration ${event.durationUs}`);
        }

        mark = nowMs();
        const videoFrame = new VideoFrameClass(canvas, { timestamp: timestampUs, duration: durationUs });
        telemetry.timingsMs.videoFrame += nowMs() - mark;
        try {
          mark = nowMs();
          encoder.encode(videoFrame, { keyFrame: globalFrame % keyFrameInterval === 0 });
          telemetry.timingsMs.encodeSubmit += nowMs() - mark;
        } finally {
          videoFrame.close();
        }
        telemetry.counts.submitted++;
        telemetry.counts.scheduled++;
        previousTimestampUs = timestampUs;
        globalFrame++;
        telemetry.peaks.encoderQueue = Math.max(
          telemetry.peaks.encoderQueue,
          encoder.encodeQueueSize,
        );

        mark = nowMs();
        await encoderGate.waitForCapacity(encoderQueueLimit);
        telemetry.timingsMs.encoderBackpressure += nowMs() - mark;
        mark = nowMs();
        await writer.waitWritable();
        telemetry.timingsMs.fifoBackpressure += nowMs() - mark;

        if (typeof hooks.afterRender === "function") {
          await hooks.afterRender({
            event,
            globalFrame: globalFrame - 1,
            component,
            gameApp,
            config,
            telemetry,
          });
        }
      },
    };

    const schedulerStart = nowMs();
    await runVirtualTimeline(timelineOptions, timelineHooks);
    telemetry.timingsMs.scheduler = nowMs() - schedulerStart;

    if (telemetry.counts.expected !== null
      && telemetry.counts.scheduled !== telemetry.counts.expected) {
      throw new Error(
        `board capture: scheduler frame invariant failed: expected ${telemetry.counts.expected}, got ${telemetry.counts.scheduled}`,
      );
    }
    if (lastAppliedTick !== totalTicks
      || telemetry.counts.appliedTicks !== totalTicks + 1) {
      throw new Error(
        `board capture: target-state invariant failed: final tick ${lastAppliedTick}, applied ${telemetry.counts.appliedTicks}, expected tick ${totalTicks}`,
      );
    }

    const heartbeat = setInterval(() => {}, 100);
    mark = nowMs();
    try {
      await encoder.flush();
    } finally {
      clearInterval(heartbeat);
    }
    telemetry.timingsMs.encoderFlush = nowMs() - mark;
    if (encoderError) throw encoderError;
    if (writer.error) throw writer.error;
    if (telemetry.counts.encoded !== telemetry.counts.submitted) {
      throw new Error(
        `board capture: encoder frame invariant failed: submitted ${telemetry.counts.submitted}, encoded ${telemetry.counts.encoded}`,
      );
    }
    if (telemetry.counts.rendered !== telemetry.counts.submitted) {
      throw new Error(
        `board capture: render frame invariant failed: rendered ${telemetry.counts.rendered}, submitted ${telemetry.counts.submitted}`,
      );
    }

    encoder.close();
    encoder = null;
    mark = nowMs();
    await writer.finish();
    telemetry.timingsMs.fifoFinish = nowMs() - mark;
    telemetry.counts.writtenBytes = writer.writtenBytes;
    if (telemetry.counts.writtenBytes !== telemetry.counts.encodedBytes) {
      throw new Error(
        `board capture: FIFO byte invariant failed: encoded ${telemetry.counts.encodedBytes}, wrote ${telemetry.counts.writtenBytes}`,
      );
    }

    if (typeof hooks.afterCapture === "function") {
      await hooks.afterCapture({ component, gameApp, app, renderer, config, telemetry });
    }
    telemetry.ok = true;
    telemetry.finishedAt = new Date().toISOString();
    telemetry.timingsMs.total = nowMs() - captureStart;
    telemetry.throughputFps = telemetry.counts.encoded / (telemetry.timingsMs.total / 1000);
    writeTelemetry(telemetryFile, telemetry);
    log("capture-complete", telemetry);
    if (doneFile) fs.writeFileSync(doneFile, "");

    if (options.closeWindow === true) {
      try {
        if (typeof window !== "undefined" && typeof window.close === "function") window.close();
      } catch (_) {}
    }
    return telemetry;
  } catch (error) {
    fail(`board capture error: ${errorText(error)}`);
    if (telemetry) {
      telemetry.error = errorText(error);
      telemetry.finishedAt = new Date().toISOString();
      telemetry.timingsMs.total = nowMs() - captureStart;
      writeTelemetry(telemetryFile, telemetry);
    }
    log("capture-failed", { error: errorText(error) });
    if (options.closeWindow === true) {
      try {
        if (typeof window !== "undefined" && typeof window.close === "function") window.close();
      } catch (_) {}
    }
    if (options.throwOnError) throw error;
    return telemetry || { ok: false, error: errorText(error) };
  } finally {
    if (encoderGate) encoderGate.destroy();
    if (encoder) {
      try { encoder.reset(); } catch (_) {}
      try { encoder.close(); } catch (_) {}
    }
    if (writer) writer.destroy();
    if (clockControl) clockControl.restore();
    if (visualLayer) visualLayer.destroy();
    if (originalRandom) Math.random = originalRandom;
    if (stateSnapshot) {
      try {
        component.play = stateSnapshot.play;
        component._tickRate = stateSnapshot.tickRate;
        if (stateSnapshot.background && component.screepsRendererRef._gameApp.app.renderer.background) {
          const background = component.screepsRendererRef._gameApp.app.renderer.background;
          background.color = stateSnapshot.background.color;
          background.alpha = stateSnapshot.background.alpha;
        }
        if (stateSnapshot.stage) {
          const currentStage = component.screepsRendererRef._gameApp.app.stage;
          if (currentStage.scale && typeof currentStage.scale.set === "function") {
            currentStage.scale.set(stateSnapshot.stage.scaleX, stateSnapshot.stage.scaleY);
          } else {
            currentStage.scale.x = stateSnapshot.stage.scaleX;
            currentStage.scale.y = stateSnapshot.stage.scaleY;
          }
          if (currentStage.position && typeof currentStage.position.set === "function") {
            currentStage.position.set(stateSnapshot.stage.positionX, stateSnapshot.stage.positionY);
          } else {
            currentStage.position.x = stateSnapshot.stage.positionX;
            currentStage.position.y = stateSnapshot.stage.positionY;
          }
        }
      } catch (_) {
        // Best effort only; a live capture window is normally reused for the
        // next replay URL, while one-shot callers may still close it.
      }
    }
  }
}

module.exports = {
  captureBoard,
  installSeededRendererRandom,
  loadVisualStates,
  materializeBlobUrls,
  materializeExternalAssetUrls,
  materializeRendererResources,
  preserveRendererRandomState,
  rendererRandomState,
  resetRendererScene,
  setRendererRandomState,
  resolveCaptureConfig,
  resolveCaptureTransport,
  resolveRendererImplementationFingerprint,
};
