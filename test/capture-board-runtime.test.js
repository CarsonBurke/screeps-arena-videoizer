"use strict";

const assert = require("node:assert/strict");
const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawnSync } = require("node:child_process");
const test = require("node:test");

const {
  captureBoard,
  installSeededRendererRandom,
  materializeBlobUrls,
  materializeRendererResources,
  preserveRendererRandomState,
  rendererRandomState,
  resetRendererScene,
  setRendererRandomState,
  resolveRendererImplementationFingerprint,
} = require("../capture-board-runtime");

class FakeTicker {
  constructor() {
    this.autoStart = true;
    this.started = true;
    this.minFPS = 10;
    this.maxFPS = 0;
    this.speed = 1;
    this.lastTime = 123;
    this.listeners = [];
    this.updates = [];
  }

  add(fn, context, priority = 0) {
    this.listeners.push({ fn, context, priority });
  }

  remove(fn, context) {
    this.listeners = this.listeners.filter((item) => item.fn !== fn || item.context !== context);
  }

  start() { this.started = true; }
  stop() { this.started = false; }

  update(time) {
    this.updates.push(time);
    for (const { fn, context } of [...this.listeners]) fn.call(context, time);
  }
}

class FakeVideoFrame {
  constructor(canvas, init) {
    this.canvas = canvas;
    this.init = init;
    this.closed = false;
  }

  close() { this.closed = true; }
}

class FakeVideoEncoder {
  static async isConfigSupported(config) {
    return { supported: true, config };
  }

  constructor(callbacks) {
    this.callbacks = callbacks;
    this.encodeQueueSize = 0;
    this.dequeueListeners = new Set();
  }

  addEventListener(name, listener) {
    if (name === "dequeue") this.dequeueListeners.add(listener);
  }

  removeEventListener(name, listener) {
    if (name === "dequeue") this.dequeueListeners.delete(listener);
  }

  configure(config) { this.config = config; }

  encode(frame) {
    assert.equal(frame.closed, false);
    this.callbacks.output({
      byteLength: 1,
      copyTo(target) { target[0] = 0x65; },
    });
  }

  async flush() {}
  close() {}
  reset() {}
}

test("materializeBlobUrls embeds session-local and remote nested assets deterministically", async () => {
  const result = await materializeBlobUrls({
    badgeUrl: "blob:file:///ephemeral-id",
    nested: ["unchanged", "https://example.invalid/floor.png"],
  }, async (url) => {
    assert.match(url, /^(?:blob:|https:)/);
    return {
      ok: true,
      headers: {
        get(name) {
          if (name !== "content-type") return null;
          return url.startsWith("blob:") ? "image/svg+xml" : "image/png";
        },
      },
      async arrayBuffer() {
        return url.startsWith("blob:") ? Buffer.from("<svg/>") : Buffer.from([1, 2, 3]);
      },
    };
  });
  assert.deepEqual(result, {
    badgeUrl: "data:image/svg+xml;base64,PHN2Zy8+",
    nested: ["unchanged", "data:image/png;base64,AQID"],
  });
});

test("materializeRendererResources makes relative and remote assets self-contained", async (t) => {
  const resourcesPath = fs.mkdtempSync(path.join(os.tmpdir(), "sca-assets-test-"));
  t.after(() => fs.rmSync(resourcesPath, { recursive: true, force: true }));
  const assetDirectory = path.join(resourcesPath, "app", "dist");
  fs.mkdirSync(assetDirectory, { recursive: true });
  fs.writeFileSync(path.join(assetDirectory, "texture.png"), Buffer.from([1, 2, 3]));

  const resources = await materializeRendererResources({
    embedded: "data:image/png;base64,AA==",
    local: "texture.png",
    remote: "https://example.invalid/icon.svg",
  }, {
    resourcesPath,
    async fetchAsset(url) {
      assert.equal(url, "https://example.invalid/icon.svg");
      return {
        ok: true,
        headers: { get: () => "image/svg+xml" },
        arrayBuffer: async () => Buffer.from("<svg/>"),
      };
    },
  });
  assert.deepEqual(resources, {
    embedded: "data:image/png;base64,AA==",
    local: "data:image/png;base64,AQID",
    remote: "data:image/svg+xml;base64,PHN2Zy8+",
  });
  await assert.rejects(
    materializeRendererResources({ unsafe: "../texture.png" }, { resourcesPath }),
    /unresolved path/,
  );
});

test("renderer implementation fingerprint uses the unpatched bundle when retained", (t) => {
  const resourcesPath = fs.mkdtempSync(path.join(os.tmpdir(), "sca-renderer-hash-test-"));
  t.after(() => fs.rmSync(resourcesPath, { recursive: true, force: true }));
  const bundleDir = path.join(resourcesPath, "app", "dist");
  fs.mkdirSync(bundleDir, { recursive: true });
  const bundlePath = path.join(bundleDir, "main.js");
  fs.writeFileSync(bundlePath, "patched bundle");
  fs.writeFileSync(`${bundlePath}.videoizer.bak`, "official bundle");

  assert.equal(
    resolveRendererImplementationFingerprint({ resourcesPath }),
    crypto.createHash("sha256").update("official bundle").digest("hex"),
  );
  assert.equal(
    resolveRendererImplementationFingerprint({
      resourcesPath,
      rendererImplementationFingerprint: "b".repeat(64),
    }),
    "b".repeat(64),
  );
});

test("installSeededRendererRandom keeps global state aligned with Math.random", (t) => {
  const key = "__screepsArenaVideoizerRandomState";
  const seedKey = "__screepsArenaVideoizerRandomSeed";
  const hadState = Object.prototype.hasOwnProperty.call(globalThis, key);
  const hadSeed = Object.prototype.hasOwnProperty.call(globalThis, seedKey);
  const previousState = globalThis[key];
  const previousSeed = globalThis[seedKey];
  const previousRandom = Math.random;
  t.after(() => {
    Math.random = previousRandom;
    if (hadState) globalThis[key] = previousState;
    else delete globalThis[key];
    if (hadSeed) globalThis[seedKey] = previousSeed;
    else delete globalThis[seedKey];
  });

  // Simulate a boot seed that differs from the replay seed, then reseed via the
  // shipped capture helper (the path used when capture-random-seed mismatches).
  globalThis[key] = 1;
  globalThis[seedKey] = "boot-seed";
  Math.random = () => 0.5;

  const seed = "replay-seed";
  installSeededRendererRandom(seed);
  assert.equal(globalThis[seedKey], seed);
  const first = Math.random();
  assert.equal(rendererRandomState(), Math.random.getState());
  assert.equal(globalThis[key], Math.random.getState());
  assert.notEqual(first, 0.5);
  const checkpoint = rendererRandomState();
  const second = Math.random();
  setRendererRandomState(checkpoint);
  assert.equal(Math.random(), second);
  assert.equal(globalThis[key], Math.random.getState());
});

test("calculation compilation preserves the renderer RNG stream", (t) => {
  const key = "__screepsArenaVideoizerRandomState";
  const hadState = Object.prototype.hasOwnProperty.call(globalThis, key);
  const previous = globalThis[key];
  t.after(() => {
    if (hadState) globalThis[key] = previous;
    else delete globalThis[key];
  });
  globalThis[key] = 123;
  const random = Math.random;
  const result = preserveRendererRandomState(() => {
    globalThis[key] = 999;
    Math.random = () => 0;
    return "compiled";
  });
  assert.equal(result, "compiled");
  assert.equal(globalThis[key], 123);
  assert.equal(Math.random, random);
  assert.throws(() => preserveRendererRandomState(() => {
    globalThis[key] = 456;
    throw new Error("failed");
  }), /failed/);
  assert.equal(globalThis[key], 123);
});

test("renderer random state falls back to a stateful seeded function", (t) => {
  const key = "__screepsArenaVideoizerRandomState";
  const hadState = Object.prototype.hasOwnProperty.call(globalThis, key);
  const previousState = globalThis[key];
  const previousRandom = Math.random;
  t.after(() => {
    Math.random = previousRandom;
    if (hadState) globalThis[key] = previousState;
    else delete globalThis[key];
  });
  delete globalThis[key];
  let state = 17;
  const random = () => 0.5;
  random.getState = () => state;
  random.setState = (value) => { state = value; };
  Math.random = random;
  assert.equal(rendererRandomState(), 17);
  setRendererRandomState(23);
  assert.equal(state, 23);
  preserveRendererRandomState(() => {
    state = 99;
  });
  assert.equal(state, 23);
});

test("renderer scene reset synchronously removes prior replay state and preserves persistent visuals", (t) => {
  class DisplayNode {
    constructor(parent = null) {
      this.parent = parent;
      this.children = [];
      this.destroyed = false;
      if (parent) parent.children.push(this);
    }

    destroy(options = {}) {
      if (this.destroyed) return;
      this.destroyed = true;
      if (options && options.children) {
        for (const child of [...this.children]) child.destroy(options);
      }
      if (this.parent) {
        this.parent.children = this.parent.children.filter((child) => child !== this);
        this.parent = null;
      }
    }
  }

  const stateKey = "__screepsArenaVideoizerRandomState";
  const hadState = Object.prototype.hasOwnProperty.call(globalThis, stateKey);
  const previousState = globalThis[stateKey];
  t.after(() => {
    if (hadState) globalThis[stateKey] = previousState;
    else delete globalThis[stateKey];
  });
  globalThis[stateKey] = 77;

  const stage = new DisplayNode();
  const layer = new DisplayNode(stage);
  const terrain = new DisplayNode(stage);
  stage.terrainObjects = { wallMask: terrain };
  const root = new DisplayNode(stage);
  const rootChild = new DisplayNode(root);
  const transientEffect = new DisplayNode(stage);
  const decorationRoot = new DisplayNode(stage);
  const decorationChild = new DisplayNode(decorationRoot);
  const actionManager = {
    actions: {
      root: { container: rootChild },
      effect: { container: transientEffect },
      terrain: { container: terrain },
      decoration: { container: decorationChild },
    },
    _actionsToDelete: [{}],
    _last: 123,
  };
  stage.actionManager = actionManager;
  let gameObjectDestroyed = 0;
  const world = {
    stage,
    layers: { objects: layer },
    terrainObjects: stage.terrainObjects,
    decorations: [{ type: "wallGraffiti" }],
    decorationsContainer: decorationRoot,
    gameObjects: {
      old: {
        rootContainer: root,
        _destroy() {
          gameObjectDestroyed++;
          globalThis[stateKey] = 999;
          root.destroy({ children: true });
        },
      },
    },
  };
  let decorationsRebuilt = 0;
  const gameApp = {
    world,
    actionManager,
    app: { stage },
    setDecorations(decorations) {
      assert.equal(decorations, world.decorations);
      assert.deepEqual(actionManager.actions, {});
      decorationRoot.destroy({ children: true });
      world.decorationsContainer = new DisplayNode(stage);
      actionManager.actions.fresh = { container: new DisplayNode(world.decorationsContainer) };
      decorationsRebuilt++;
      globalThis[stateKey] = 888;
    },
  };

  assert.deepEqual(resetRendererScene(gameApp), {
    gameObjects: 1,
    actions: 4,
    transientActionContainers: 1,
    decorationsRebuilt: true,
  });
  assert.equal(gameObjectDestroyed, 1);
  assert.deepEqual(world.gameObjects, {});
  assert.equal(root.destroyed, true);
  assert.equal(transientEffect.destroyed, true);
  assert.equal(decorationRoot.destroyed, true);
  assert.equal(terrain.destroyed, false);
  assert.equal(layer.destroyed, false);
  assert.equal(stage.destroyed, false);
  assert.equal(decorationsRebuilt, 1);
  assert.deepEqual(Object.keys(actionManager.actions), ["fresh"]);
  assert.deepEqual(actionManager._actionsToDelete, []);
  assert.equal(actionManager._last, 0);
  assert.equal(globalThis[stateKey], 77);
});

test("captureBoard drives ticks and frames exactly without implicit animation", async (t) => {
  const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "sca-capture-test-"));
  t.after(() => fs.rmSync(tempDir, { recursive: true, force: true }));
  const fifoPath = path.join(tempDir, "capture.fifo");
  const mkfifo = spawnSync("mkfifo", [fifoPath], { encoding: "utf8" });
  assert.equal(mkfifo.status, 0, mkfifo.stderr);
  const fifoFd = fs.openSync(fifoPath, fs.constants.O_RDWR | fs.constants.O_NONBLOCK);
  t.after(() => fs.closeSync(fifoFd));

  const appTicker = new FakeTicker();
  FakeTicker.shared = new FakeTicker();
  const stage = {
    scale: { x: 1, y: 1, set(x, y = x) { this.x = x; this.y = y; } },
    position: { x: 0, y: 0, set(x, y) { this.x = x; this.y = y; } },
    terrainObjects: {
      previousWalls: [{ type: "wall", x: 1, y: 2 }],
      previousSwamps: [{ type: "swamp", x: 3, y: 4 }],
      swampObjects: [
        null,
        null,
        null,
        { tilePosition: { x: 17, y: 19 } },
        { tilePosition: { x: -23, y: -29, set(x, y) { this.x = x; this.y = y; } } },
      ],
    },
  };
  const canvas = { width: 64, height: 64 };
  let explicitRenders = 0;
  let visualRenders = 0;
  let implicitRenders = 0;
  let implicitAnimates = 0;
  const app = {
    ticker: appTicker,
    stage,
    renderer: {
      view: canvas,
      width: 64,
      height: 64,
      background: { color: 0, alpha: 0 },
      render(target) {
        if (target === stage) explicitRenders++;
        else visualRenders++;
      },
    },
    render() { implicitRenders++; },
  };
  const appliedTicks = [];
  const actionDurations = [];
  const stateChunkEnds = [];
  const visualChunkEnds = [];
  let activeStateChunks = 0;
  let peakStateChunks = 0;
  const actionManager = {
    actions: {},
    _actionsToDelete: [],
    _last: 0,
    update(duration) { actionDurations.push(duration); },
  };
  const gameApp = {
    app,
    actionManager,
    applyState(state) { appliedTicks.push(state.tick); },
    animate() { implicitAnimates++; },
    animateCheckerTimer: null,
    world: {
      stage,
      gameObjects: {},
      decorations: [],
      options: { VIEW_BOX: 1000 },
      resourceMap: { creep: "data:image/svg+xml;base64,PHN2Zy8+" },
      metadata: {
        objects: {
          creep: {
            processors: [{ type: "sprite" }],
            actions: [{ actions: [{ action: "MoveTo" }] }],
          },
        },
      },
    },
  };
  appTicker.add(app.render, app, -50);
  appTicker.add(gameApp.animate, gameApp, 0);

  const component = {
    play: true,
    _tickRate: 0.2,
    screepsRendererRef: { _gameApp: gameApp },
    _scaReplayStateService: {
      chunks: {},
      async getChunk(endTick) {
        stateChunkEnds.push(endTick);
        activeStateChunks++;
        peakStateChunks = Math.max(peakStateChunks, activeStateChunks);
        await Promise.resolve();
        activeStateChunks--;
        return [];
      },
      async getTick(tick) { return { tick, objects: [] }; },
    },
    _scaReplayVisualService: {
      async getChunk(endTick) {
        visualChunkEnds.push(endTick);
        return {};
      },
      async getTick(tick) { return [{ tick }]; },
    },
    isOwner$: {
      subscribe(observer) {
        observer.next(true);
        return { unsubscribe() {} };
      },
    },
    screepsRendererVisualRef: {
      canvasRef: { nativeElement: { width: 64, height: 64 } },
      setVisual() {},
    },
    ticks$: { getValue() { return 2; } },
  };
  const originalPixi = globalThis.PIXI;
  const originalWindow = globalThis.window;
  const originalRandom = Math.random;
  let capturedReplayIR;
  let closedWindows = 0;
  globalThis.window = { close() { closedWindows++; } };
  globalThis.PIXI = {
    Texture: {
      from() { return { baseTexture: { update() {} } }; },
    },
    Sprite: class {},
    Container: class {
      addChild() {}
      destroy() {}
    },
  };
  t.after(() => {
    globalThis.PIXI = originalPixi;
    if (originalWindow === undefined) delete globalThis.window;
    else globalThis.window = originalWindow;
  });

  const telemetry = await captureBoard(component, {
    fifoPath,
    metaPath: path.join(tempDir, "capture.meta"),
    doneFile: path.join(tempDir, "capture.done"),
    errorFile: path.join(tempDir, "capture.error"),
    debugFile: path.join(tempDir, "capture.log"),
    telemetryFile: path.join(tempDir, "capture.telemetry.json"),
    VideoEncoder: FakeVideoEncoder,
    VideoFrame: FakeVideoFrame,
    width: 64,
    height: 64,
    fps: 2,
    ticksPerSecond: 1,
    framesPerTick: 2,
    simulationFps: 2,
    bitrate: 100_000,
    boardPadding: 2,
    requireMapper: false,
    compileReplayIR: true,
    rendererVersion: "1.6.8-arena",
    replayIRFile: path.join(tempDir, "capture.replay-ir.json"),
    onReplayIR(value) { capturedReplayIR = value; },
    throwOnError: true,
  });

  assert.equal(telemetry.ok, true);
  assert.equal(closedWindows, 0);
  assert.deepEqual(appliedTicks, [0, 1, 2]);
  assert.equal(explicitRenders, 5);
  assert.equal(visualRenders, 5);
  assert.equal(implicitRenders, 0);
  assert.equal(implicitAnimates, 0);
  assert.equal(actionDurations.length, 4);
  assert.deepEqual(stateChunkEnds, [0, 2]);
  assert.equal(peakStateChunks, 2);
  assert.deepEqual(visualChunkEnds, [2]);
  assert.deepEqual(telemetry.stateChunks, { count: 2, concurrency: 4 });
  assert.deepEqual(telemetry.visualChunks, { count: 1, concurrency: 4 });
  assert.deepEqual(telemetry.sceneReset, {
    gameObjects: 0,
    actions: 0,
    transientActionContainers: 0,
    decorationsRebuilt: false,
  });
  assert.equal(telemetry.replayIR.entities, 0);
  assert.equal(capturedReplayIR.replay.totalTicks, 2);
  assert.equal(capturedReplayIR.replay.randomSeed, "screeps-arena-videoizer");
  assert.equal(Number.isInteger(capturedReplayIR.replay.randomStateAtFirstTick), true);
  assert.equal(capturedReplayIR.replay.timeline.substepsPerSecond, "2");
  assert.equal(capturedReplayIR.replay.timeline.tickTransitionSeconds, "1");
  assert.equal(capturedReplayIR.replay.calculationOutputs.enabled, true);
  assert.equal(capturedReplayIR.replay.rendererGraph.enabled, true);
  assert.deepEqual(capturedReplayIR.replay.rendererGraph.columns, [[], [], [], []]);
  assert.deepEqual(capturedReplayIR.rendererContract.terrain, [
    { type: "wall", x: 1, y: 2 },
    { type: "swamp", x: 3, y: 4 },
  ]);
  assert.equal(telemetry.replayIR.rendererEvents, 0);
  assert.deepEqual(
    [...require("../replay-ir").reconstructReplayCalculations(
      capturedReplayIR.replay,
      0,
    )],
    [],
  );
  assert.equal(capturedReplayIR.replay.visualOverlay.enabled, true);
  assert.deepEqual(capturedReplayIR.replay.visualOverlay.states[1], [[], [{ tick: 1 }], []]);
  assert.equal(
    JSON.parse(fs.readFileSync(path.join(tempDir, "capture.replay-ir.json"))).replay.fingerprint,
    telemetry.replayIR.fingerprint,
  );
  assert.deepEqual(telemetry.counts, {
    scheduled: 5,
    expected: 5,
    submitted: 5,
    encoded: 5,
    rendered: 5,
    appliedTicks: 3,
    actionSubsteps: 4,
    encodedBytes: 5,
    visualTicks: 3,
    resetAnimatedSprites: 2,
    writtenBytes: 5,
  });
  assert.deepEqual(stage.terrainObjects.swampObjects[3].tilePosition, { x: 0, y: 0 });
  assert.equal(stage.terrainObjects.swampObjects[4].tilePosition.x, 0);
  assert.equal(stage.terrainObjects.swampObjects[4].tilePosition.y, 0);
  assert.equal(component.play, true);
  assert.equal(component._tickRate, 0.2);
  assert.equal(Math.random, originalRandom);
  assert.equal(appTicker.listeners.some((item) => item.fn === app.render), true);
  assert.equal(appTicker.listeners.some((item) => item.fn === gameApp.animate), true);
});

test("capture URL cannot select filesystem paths or traverse capture IDs", async (t) => {
  const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "sca-capture-security-test-"));
  t.after(() => fs.rmSync(tempDir, { recursive: true, force: true }));
  const safeError = path.join(tempDir, "safe.error");
  const injectedError = path.join(tempDir, "injected.error");

  const result = await captureBoard(null, {
    params: new URLSearchParams(
      `capture-id=../escape&capture-error=${encodeURIComponent(injectedError)}`,
    ),
    errorFile: safeError,
  });

  assert.equal(result.ok, false);
  assert.match(fs.readFileSync(safeError, "utf8"), /invalid capture-id/);
  assert.equal(fs.existsSync(injectedError), false);
});
