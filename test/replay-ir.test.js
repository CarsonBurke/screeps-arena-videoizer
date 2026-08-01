"use strict";

const assert = require("node:assert/strict");
const crypto = require("node:crypto");
const test = require("node:test");

const {
  assertRendererContractSupported,
  compileRendererContract,
  compileReplayIR,
  extractRendererWorldOptions,
  fingerprint,
  normalizeRendererMetadata,
  reconstructReplayCalculations,
  reconstructRendererEvents,
  reconstructReplayTick,
  reconstructVisualTick,
  stableStringify,
  validateRendererContract,
  validateReplayArtifact,
  validateReplayIR,
} = require("../replay-ir");

function sampleStates() {
  return [
    {
      tick: 0,
      gameTime: 100,
      users: { one: { username: "alpha" } },
      objects: [
        {
          _id: "creep-1",
          type: "creep",
          x: 10,
          y: 20,
          hits: 100,
          actionLog: {},
        },
        { room: "W0N0", type: "source", x: 5, y: 5, energy: 300 },
      ],
    },
    {
      tick: 1,
      gameTime: 101,
      users: { one: { username: "alpha" } },
      objects: [
        { room: "W0N0", type: "source", x: 5, y: 5, energy: 298 },
        {
          _id: "creep-1",
          type: "creep",
          x: 11,
          y: 20,
          hits: 100,
          actionLog: { harvest: { x: 5, y: 5 }, say: false },
        },
      ],
    },
    {
      tick: 2,
      gameTime: 102,
      users: { one: { username: "alpha" } },
      objects: [
        {
          _id: "creep-1",
          type: "creep",
          x: 11,
          y: 20,
          actionLog: { attacked: true },
        },
        { _id: "effect-1", type: "areaEffect", x: 11, y: 20, duration: 3 },
      ],
    },
    {
      tick: 3,
      gameTime: 103,
      users: { one: { username: "alpha" } },
      objects: [
        { room: "W0N0", type: "source", x: 5, y: 5, energy: 297 },
      ],
    },
  ];
}

function resign(value) {
  const { fingerprint: _oldFingerprint, ...payload } = value;
  value.fingerprint = crypto.createHash("sha256")
    .update(JSON.stringify(payload))
    .digest("hex");
}

test("ReplayIR reconstructs every source tick exactly", () => {
  const states = sampleStates();
  const replay = compileReplayIR({
    states,
    framesPerSecond: 30,
    ticksPerSecond: "15/4",
    substepsPerSecond: 60,
    renderConfig: {
      width: 640,
      height: 480,
      backgroundColor: 0x191B21,
      boardFrame: {
        mode: "auto",
        outputWidth: 640,
        outputHeight: 480,
        boardWidth: 10_000,
        boardHeight: 10_000,
        worldMinX: -50,
        worldMinY: -50,
        pivotX: -50,
        pivotY: -50,
        zoom: 0.0448,
        x: 96,
        y: 16,
        left: 96,
        top: 16,
        right: 544,
        bottom: 464,
        width: 448,
        height: 448,
        padding: 16,
        panX: 0,
        panY: 0,
      },
    },
    randomSeed: "arena-123",
    randomStateAtFirstTick: 0xffffffff,
    visualOverlayEnabled: true,
    visualStates: [
      [],
      [{ type: "circle", x: 10, y: 20 }],
      [{ type: "line", x1: 0, y1: 0, x2: 10, y2: 20 }],
      [],
    ],
  });

  assert.equal(replay.totalTicks, 3);
  assert.equal(replay.entities.length, 3);
  assert.deepEqual(replay.actionEvents, [
    [1, "creep-1", "harvest"],
    [2, "creep-1", "attacked"],
  ]);
  assert.equal(replay.randomSeed, "arena-123");
  assert.equal(replay.randomStateAtFirstTick, 0xffffffff);
  assert.equal(replay.renderConfig.boardFrame.right, 544);
  assert.equal(replay.visualOverlay.enabled, true);
  assert.deepEqual(reconstructVisualTick(replay, 1), [{ type: "circle", x: 10, y: 20 }]);
  for (let tick = 0; tick < states.length; tick++) {
    assert.deepEqual(reconstructReplayTick(replay, tick), states[tick]);
  }
});

test("ReplayIR preserves disappearance, reappearance, ordering, and missing properties", () => {
  const states = sampleStates();
  const replay = compileReplayIR({ states });
  const source = replay.entities.find(({ id }) => id === "W0N0:source:5:5");

  assert.deepEqual(source.lifetimes, [[0, 2], [3, 4]]);
  assert.deepEqual(reconstructReplayTick(replay, 1).objects.map((object) => object.type), [
    "source",
    "creep",
  ]);
  assert.equal("hits" in reconstructReplayTick(replay, 2).objects[0], false);
});

test("ReplayIR stores exact calculation tracks across changes and lifetimes", () => {
  const states = sampleStates();
  const calculationStates = states.map((state, tick) => new Map(
    state.objects.map((object) => {
      const id = object._id || `${object.room}:${object.type}:${object.x}:${object.y}`;
      return [id, {
        stable: object.type,
        value: tick === 0 ? undefined : object.hits,
      }];
    }),
  ));
  const replay = compileReplayIR({ states, calculationStates });

  assert.equal(replay.calculationOutputs.enabled, true);
  assert.deepEqual(
    [...reconstructReplayCalculations(replay, 0)],
    [
      ["creep-1", { stable: "creep", value: undefined }],
      ["W0N0:source:5:5", { stable: "source", value: undefined }],
    ],
  );
  assert.deepEqual(
    [...reconstructReplayCalculations(replay, 2)],
    [
      ["creep-1", { stable: "creep", value: undefined }],
      ["effect-1", { stable: "areaEffect", value: undefined }],
    ],
  );
  assert.deepEqual(
    [...reconstructReplayCalculations(replay, 3)],
    [["W0N0:source:5:5", { stable: "source", value: undefined }]],
  );
});

test("ReplayIR calculation tracks require complete active-object coverage", () => {
  const states = [{ objects: [{ _id: "one", type: "creep" }] }];
  assert.throws(
    () => compileReplayIR({ states, calculationStates: [new Map()] }),
    /missing active object one/,
  );
  assert.throws(
    () => compileReplayIR({
      states,
      calculationStates: [new Map([
        ["one", {}],
        ["inactive", {}],
      ])],
    }),
    /references inactive object inactive/,
  );
  const replay = compileReplayIR({ states });
  assert.equal(replay.calculationOutputs.enabled, false);
  assert.throws(
    () => reconstructReplayCalculations(replay, 0),
    /does not contain compiled calculation outputs/,
  );
});

test("ReplayIR streams calculation evaluation without retaining tick maps", () => {
  const states = sampleStates();
  const evaluatedTicks = [];
  const replay = compileReplayIR({
    states,
    calculationEvaluator(state, tick) {
      evaluatedTicks.push(tick);
      return new Map(state.objects.map((object) => [
        object._id || `${object.room}:${object.type}:${object.x}:${object.y}`,
        { tick, type: object.type },
      ]));
    },
  });
  assert.deepEqual(evaluatedTicks, [0, 1, 2, 3]);
  assert.deepEqual(
    reconstructReplayCalculations(replay, 3).get("W0N0:source:5:5"),
    { tick: 3, type: "source" },
  );
  assert.throws(
    () => compileReplayIR({
      states,
      calculationStates: states.map(() => new Map()),
      calculationEvaluator() {},
    }),
    /mutually exclusive/,
  );
  assert.throws(
    () => compileReplayIR({
      states,
      calculationEvaluator() {},
    }),
    /calculation state 0 must be a Map/,
  );
});

test("ReplayIR indexes canonical processor graph events by tick", () => {
  const states = sampleStates();
  const evaluatedTicks = [];
  const replay = compileReplayIR({
    states,
    rendererEventEvaluator(state, tick, calculations) {
      assert.equal(calculations, null);
      evaluatedTicks.push(tick);
      if (tick === 0) {
        return [
          [0, null, "preprocessor:run", "terrain", null],
          [0, "creep-1", "object:create", null, { z: undefined, a: 1 }],
        ];
      }
      if (tick === 2) {
        return [[2, "creep-1", "processor:run", "sprite", {
          actions: [],
          type: "sprite",
        }]];
      }
      return [];
    },
  });
  assert.deepEqual(evaluatedTicks, [0, 1, 2, 3]);
  assert.equal(replay.rendererGraph.enabled, true);
  assert.deepEqual(replay.rendererGraph.offsets, [0, 2, 2, 3, 3]);
  assert.deepEqual(reconstructRendererEvents(replay, 0), [
    [0, null, "preprocessor:run", "terrain", null],
    [0, "creep-1", "object:create", null, {
      a: 1,
      z: { $undefined: true },
    }],
  ]);
  assert.deepEqual(reconstructRendererEvents(replay, 1), []);
  assert.deepEqual(reconstructRendererEvents(replay, 2), [
    [2, "creep-1", "processor:run", "sprite", {
      actions: [],
      type: "sprite",
    }],
  ]);

  assert.throws(() => compileReplayIR({
    states,
    rendererEventEvaluator() {},
  }), /must be an array/);
  assert.throws(() => compileReplayIR({
    states,
    rendererEventEvaluator(_state, tick) {
      return [[tick, "missing", "object:create", null, null]];
    },
  }), /invalid renderer event/);
  assert.throws(() => compileReplayIR({
    states,
    rendererEventEvaluator(_state, tick) {
      return [[tick, null, "unknown:event", null, null]];
    },
  }), /invalid renderer event/);
  assert.throws(() => compileReplayIR({
    states,
    rendererEventEvaluator(_state, tick) {
      return [[tick, null, "processor:run", null, null]];
    },
  }), /invalid renderer event/);
  assert.throws(() => compileReplayIR({
    states,
    rendererEventEvaluator(_state, tick) {
      return [[tick, "creep-1", "preprocessor:run", "terrain", null]];
    },
  }), /invalid renderer event/);
});

test("ReplayIR accepts one fused calculation and renderer-event evaluation", () => {
  const replay = compileReplayIR({
    states: [{ objects: [{ _id: "one", type: "creep" }] }],
    rendererTickEvaluator(_state, tick) {
      return {
        calculations: new Map([["one", { value: 7 }]]),
        events: [[tick, "one", "object:create", null, null]],
      };
    },
  });
  assert.equal(replay.calculationOutputs.enabled, true);
  assert.equal(replay.rendererGraph.enabled, true);
  assert.deepEqual(
    [...reconstructReplayCalculations(replay, 0)],
    [["one", { value: 7 }]],
  );
  assert.throws(() => compileReplayIR({
    states: [{ objects: [] }],
    calculationEvaluator: () => new Map(),
    rendererTickEvaluator: () => ({ calculations: new Map(), events: [] }),
  }), /mutually exclusive/);
});

test("ReplayIR distinguishes explicit undefined renderer fields from absent fields", () => {
  const replay = compileReplayIR({
    states: [
      { objects: [{ _id: "neutral", type: "source", user: undefined }] },
      { objects: [{ _id: "neutral", type: "source" }] },
    ],
  });
  const first = reconstructReplayTick(replay, 0).objects[0];
  const second = reconstructReplayTick(replay, 1).objects[0];
  assert.equal("user" in first, true);
  assert.equal(first.user, undefined);
  assert.equal("user" in second, false);
});

test("ReplayIR rejects duplicate renderer identities instead of dropping an object", () => {
  assert.throws(() => compileReplayIR({
    states: [{
      objects: [
        { room: "W0N0", type: "road", x: 1, y: 2 },
        { room: "W0N0", type: "road", x: 1, y: 2 },
      ],
    }],
  }), /duplicate object identity/);
});

test("canonical hashes are stable across object insertion order", () => {
  const left = { z: [{ b: 2, a: 1 }], a: true };
  const right = { a: true, z: [{ a: 1, b: 2 }] };
  assert.equal(stableStringify(left), stableStringify(right));
  assert.equal(fingerprint(left), fingerprint(right));
});

test("canonicalize modes share the walker and differ only on non-JSON leaves", () => {
  const {
    canonicalize,
    canonicalizeJSON,
    canonicalizeRendererEventValue,
    canonicalizeValue,
  } = require("../replay-ir");

  assert.deepEqual(canonicalize({ b: 2, a: undefined, f: () => 1, n: 1n }), {
    a: { $undefined: true },
    b: 2,
    f: { $function: "() => 1" },
    n: { $bigint: "1" },
  });
  assert.deepEqual(
    canonicalizeValue({ b: 2, a: 1 }, "full"),
    canonicalize({ b: 2, a: 1 }),
  );
  assert.throws(() => canonicalizeJSON({ a: undefined }), /non-JSON undefined/);
  assert.throws(() => canonicalizeJSON({ f: () => 1 }), /non-JSON function/);
  assert.throws(() => canonicalizeJSON({ n: 1n }), /non-JSON bigint/);
  assert.deepEqual(canonicalizeRendererEventValue({ a: undefined, b: 1 }), {
    a: { $undefined: true },
    b: 1,
  });
  assert.throws(
    () => canonicalizeRendererEventValue({ f: () => 1 }),
    /non-event function/,
  );
  assert.throws(
    () => canonicalizeRendererEventValue({ n: 1n }),
    /non-event bigint/,
  );
  assert.equal(canonicalize(-0), 0);
  assert.throws(() => canonicalize(Number.NaN), /non-finite/);
  const cyclic = {};
  cyclic.self = cyclic;
  assert.throws(() => canonicalizeJSON(cyclic), /cyclic/);
});

test("renderer contract inventories nested processors and actions", () => {
  const metadata = {
    preprocessors: ["terrain"],
    objects: {
      creep: {
        calculations: [{ id: "rotation", func: ({ state }) => state.x }],
        processors: [{
          type: "sprite",
          actions: [{
            action: "Repeat",
            params: [{
              action: "Sequence",
              params: [[{ action: "AlphaTo", params: [0, 1] }]],
            }],
          }],
        }],
        actions: [{
          id: "move",
          actions: [{ action: "Ease", params: [{ action: "MoveTo" }] }],
        }],
        disappearProcessor: { type: "disappear" },
      },
    },
  };
  const contract = compileRendererContract({
    rendererVersion: "1.6.8-arena",
    metadata,
    resources: { creep: "creep.svg" },
    decorations: [{ type: "floorLandscape", fill: "#123456" }],
    terrain: [{ type: "wall", x: 1, y: 2 }],
    worldOptions: { CELL_SIZE: 100 },
  });

  assert.deepEqual(contract.inventory, {
    objectTypes: ["creep"],
    processorTypes: ["disappear", "sprite"],
    actionTypes: ["AlphaTo", "Ease", "MoveTo", "Repeat", "Sequence"],
    preprocessors: ["terrain"],
    calculationIds: ["rotation"],
    drawingMethods: [],
    expressionOperators: [],
    functionSemantics: [
      `func:${fingerprint("({ state }) => state.x")}`,
    ],
    layerIds: [],
    rendererImplementationFingerprints: [],
  });
  assert.equal(contract.metadata.objects.creep.calculations[0].func.$function.includes("state.x"), true);
  assert.deepEqual(contract.decorations, [{ fill: "#123456", type: "floorLandscape" }]);
  assert.deepEqual(contract.terrain, [{ type: "wall", x: 1, y: 2 }]);
});

test("renderer contract requires array decorations and terrain before verification caching", () => {
  assert.throws(
    () => compileRendererContract({ metadata: { objects: {} }, decorations: {} }),
    /decorations must be an array/,
  );
  assert.throws(
    () => compileRendererContract({ metadata: { objects: {} }, terrain: {} }),
    /terrain must be an array/,
  );
});

test("renderer contract fails closed when the backend lacks a semantic", () => {
  const contract = compileRendererContract({
    metadata: {
      objects: {
        creep: {
          processors: [{ type: "sprite" }, { type: "creepActions" }],
          actions: [{ actions: [{ action: "MoveTo" }] }],
        },
      },
    },
  });
  assert.throws(() => assertRendererContractSupported(contract, {
    objectTypes: ["creep"],
    processorTypes: ["sprite"],
    actionTypes: ["MoveTo"],
    preprocessors: [],
    calculationIds: [],
    drawingMethods: [],
    expressionOperators: [],
    functionSemantics: [],
    layerIds: [],
    rendererImplementationFingerprints: [],
  }), /processorTypes: creepActions/);
  assert.equal(assertRendererContractSupported(contract, {
    objectTypes: ["creep"],
    processorTypes: ["sprite", "creepActions"],
    actionTypes: ["MoveTo"],
    preprocessors: [],
    calculationIds: [],
    drawingMethods: [],
    expressionOperators: [],
    functionSemantics: [],
    layerIds: [],
    rendererImplementationFingerprints: [],
  }), true);
});

test("renderer support fails closed on calculations, drawing methods, expressions, and layers", () => {
  const contract = compileRendererContract({
    metadata: {
      layers: [{ id: "effects" }],
      objects: {
        creep: {
          calculations: [{ id: "direction", func: ({ state }) => state.x }],
          processors: [{
            type: "draw",
            payload: {
              alpha: { $calc: "direction" },
              drawings: [{ method: "drawStar", params: [] }],
            },
          }],
        },
      },
    },
  });
  const support = Object.fromEntries(
    Object.entries(contract.inventory).map(([key, values]) => [key, [...values]]),
  );
  support.drawingMethods = [];
  assert.throws(
    () => assertRendererContractSupported(contract, support),
    /drawingMethods: drawStar/,
  );
});

test("renderer contracts fingerprint scene-filter functions instead of dropping them", () => {
  const objectFilter = (objects) => objects.filter(({ type }) => type !== "hidden");
  const contract = compileRendererContract({
    metadata: { objects: {} },
    worldOptions: { objectFilter, CELL_SIZE: 100 },
  });
  const semantic = `objectFilter:${fingerprint(
    Function.prototype.toString.call(objectFilter),
  )}`;
  assert.deepEqual(contract.worldOptions, { CELL_SIZE: 100 });
  assert.deepEqual(contract.inventory.functionSemantics, [semantic]);
  const support = Object.fromEntries(
    Object.entries(contract.inventory).map(([key, values]) => [key, [...values]]),
  );
  support.functionSemantics = [];
  assert.throws(
    () => assertRendererContractSupported(contract, support),
    new RegExp(`functionSemantics: ${semantic}`),
  );
});

test("renderer implementation fingerprints are valid and participate in support checks", () => {
  assert.throws(() => compileRendererContract({
    metadata: { objects: {} },
    rendererImplementationFingerprint: "not-a-digest",
  }), /lowercase SHA-256/);

  const implementationFingerprint = "a".repeat(64);
  const contract = compileRendererContract({
    metadata: { objects: {} },
    rendererImplementationFingerprint: implementationFingerprint,
  });
  const support = Object.fromEntries(
    Object.entries(contract.inventory).map(([key, values]) => [key, [...values]]),
  );
  support.rendererImplementationFingerprints = [];
  assert.throws(
    () => assertRendererContractSupported(contract, support),
    new RegExp(`rendererImplementationFingerprints: ${implementationFingerprint}`),
  );
});

test("renderer contracts reject malformed function semantic fingerprints", () => {
  const contract = compileRendererContract({
    metadata: { objects: {} },
  });
  const malformed = JSON.parse(JSON.stringify(contract));
  malformed.inventory.functionSemantics = ["objectFilter:not-a-digest"];
  resign(malformed);
  assert.throws(
    () => validateRendererContract(malformed),
    /invalid renderer function semantic/,
  );
});

test("renderer metadata normalizes runtime initialization and generated ids", () => {
  assert.deepEqual(normalizeRendererMetadata({
    objects: {
      creep: {
        _initialized: true,
        processors: [{ id: "id#93172", type: "sprite" }],
      },
    },
  }), {
    objects: {
      creep: {
        processors: [{ id: "auto:$.objects.creep.processors[0]", type: "sprite" }],
      },
    },
  });
});

test("ReplayIR is deeply immutable and loaded artifacts are tamper-evident", () => {
  const contract = compileRendererContract({
    metadata: { objects: { creep: { processors: [{ type: "sprite" }] } } },
  });
  const replay = compileReplayIR({
    states: sampleStates(),
    rendererContract: contract,
  });
  assert.equal(Object.isFrozen(replay.entities[0].properties), true);
  assert.throws(() => {
    replay.entities[0].properties.type[1][0] = "tampered";
  }, /read only|extensible|assign/i);
  assert.equal(validateReplayArtifact({ rendererContract: contract, replay }), true);

  const loaded = JSON.parse(JSON.stringify(replay));
  loaded.entities[0].properties.type[1][0] = "tampered";
  assert.throws(() => validateReplayIR(loaded), /fingerprint mismatch/);
});

test("ReplayIR rejects executable replay values and zero rational denominators", () => {
  assert.throws(
    () => compileReplayIR({ states: [{ objects: [], invalid() {} }] }),
    /non-JSON function/,
  );
  assert.throws(
    () => compileReplayIR({ states: [{ objects: [] }], framesPerSecond: "1/0" }),
    /positive decimal or rational/,
  );
  assert.throws(
    () => compileReplayIR({
      states: [{ objects: [] }],
      randomStateAtFirstTick: 0x1_0000_0000,
    }),
    /unsigned 32-bit/,
  );
});

test("ReplayIR rejects nested non-JSON values even when raw JSON would collide", () => {
  for (const invalid of [
    { nested: { value: undefined } },
    { nested: { method() {} } },
    { nested: [undefined] },
  ]) {
    assert.throws(() => compileReplayIR({
      states: [
        { objects: [{ _id: "one", payload: invalid.nested instanceof Array ? [null] : {} }] },
        { objects: [{ _id: "one", payload: invalid.nested }] },
      ],
    }), /non-JSON/);
  }
});

test("loaded replay and contract validation require the complete current schema", () => {
  const contract = compileRendererContract({
    metadata: { objects: { creep: { processors: [{ type: "sprite" }] } } },
  });
  const invalidContract = JSON.parse(JSON.stringify(contract));
  invalidContract.inventory = {};
  resign(invalidContract);
  assert.throws(() => assertRendererContractSupported(invalidContract, {}), /contract structure/);
  for (const required of ["decorations", "terrain"]) {
    const missing = JSON.parse(JSON.stringify(contract));
    delete missing[required];
    resign(missing);
    assert.throws(() => validateRendererContract(missing), /contract structure/);
  }

  const replay = compileReplayIR({ states: [{ objects: [] }] });
  const invalidReplay = JSON.parse(JSON.stringify(replay));
  invalidReplay.timeline = null;
  resign(invalidReplay);
  assert.throws(() => validateReplayIR(invalidReplay), /timeline/);
});

test("visual reconstruction verifies and freezes loaded artifacts once", () => {
  const source = compileReplayIR({
    states: [{ objects: [] }],
    visualOverlayEnabled: true,
    visualStates: [[{ type: "circle", x: 1 }]],
  });
  const loaded = JSON.parse(JSON.stringify(source));
  assert.deepEqual(reconstructVisualTick(loaded, 0), [{ type: "circle", x: 1 }]);
  assert.equal(Object.isFrozen(loaded), true);
  assert.throws(() => {
    loaded.visualOverlay.states[1][0][0].x = 2;
  }, /read only|extensible|assign/i);

  const tampered = JSON.parse(JSON.stringify(source));
  tampered.visualOverlay.states[1][0][0].x = 2;
  assert.throws(() => reconstructVisualTick(tampered, 0), /fingerprint mismatch/);
});

test("renderer contract excludes only known live world infrastructure", () => {
  const live = {
    actionManager: { actions: { one: { bounds: Infinity } } },
    app: { renderer: {} },
    logger() {},
    objectFilter() {},
    resourceMap: { duplicated: "asset.svg" },
    CELL_SIZE: 100,
    size: { width: 2048, height: 2048 },
  };
  assert.deepEqual(extractRendererWorldOptions(live), {
    CELL_SIZE: 100,
    size: { height: 2048, width: 2048 },
  });
  assert.throws(
    () => extractRendererWorldOptions({ unexpectedRuntimeHandle: { bounds: Infinity } }),
    /non-finite/,
  );
});
