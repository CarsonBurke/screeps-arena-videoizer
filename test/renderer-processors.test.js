"use strict";

const assert = require("node:assert/strict");
const test = require("node:test");

const {
  PROCESSOR_EVENT_OPS,
  RendererGraphEvaluator,
  RendererProcessorEvaluator,
} = require("../renderer-processors");
const { createRendererRandom } = require("../renderer-random");

function calculationState(id, alpha) {
  return new Map([[id, { alpha }]]);
}

test("processor graph emits exact creation-order lifecycle and late-bound action events", () => {
  let randomCalls = 0;
  const evaluator = new RendererProcessorEvaluator({
    metadata: {
      preprocessors: ["terrain"],
      objects: {
        _all: {
          data: { x: { $state: "x", koef: 100 } },
          processors: [{
            id: "common",
            once: true,
            type: "container",
            payload: { id: "common-node" },
          }],
        },
        unit: {
          actions: [{
            id: "pulse",
            targetId: "body",
            props: ["value"],
            actions: [{
              action: "MoveTo",
              params: [{ $state: "x" }, 2, { $processorParam: "tickDuration" }],
            }],
          }],
          processors: [{
            id: "body-processor",
            type: "sprite",
            props: ["enabled"],
            when: ({ state }) => state.enabled,
            payload: {
              id: "body",
              texture: "unit",
              alpha: { $calc: "alpha" },
            },
            actions: [{
              action: "Sequence",
              params: [[
                { action: "RotateTo", params: [{ $random: 4 }, 0] },
                {
                  action: "ScaleTo",
                  params: [
                    { $rel: "scale.x", koef: 1.2 },
                    { $rel: "scale.y", koef: 1.2 },
                    1,
                  ],
                },
              ]],
            }],
          }],
        },
      },
    },
    world: { options: {} },
    random() {
      randomCalls++;
      return 0.25;
    },
    getRandomState: () => 0,
    setRandomState() {},
  });

  const tick0 = evaluator.evaluateTick({
    objects: [{ _id: "u1", type: "unit", x: 3, value: 1, enabled: true }],
  }, 0, 1, calculationState("u1", 0.75));
  assert.deepEqual(tick0.map((event) => event[2]), [
    "preprocessor:run",
    "object:create",
    "processor:run",
    "processor:run",
  ]);
  assert.equal(tick0[1][4], null);
  assert.equal(tick0[2][3], "auto:$.objects.unit.processors[0]");
  assert.equal(tick0[2][4], null);
  assert.equal(tick0[3][3], "auto:$.objects.unit.processors[1]");
  assert.equal(tick0[3][4], null);
  // Random action expressions are deliberately resolved by the native action
  // runtime from ReplayIR's initial PRNG checkpoint, not duplicated per event.
  assert.equal(randomCalls, 0);

  const tick1 = evaluator.evaluateTick({
    objects: [{ _id: "u1", type: "unit", x: 4, value: 2, enabled: true }],
  }, 1, 1, calculationState("u1", 0.75));
  assert.deepEqual(tick1.map((event) => event[2]), [
    "preprocessor:run",
    "action:run",
  ]);
  assert.equal(tick1[1][3], "auto:$.objects.unit.actions[0]");
  assert.equal(tick1[1][4], null);

  const tick2 = evaluator.evaluateTick({
    objects: [{ _id: "u1", type: "unit", x: 5, value: 3, enabled: false }],
  }, 2, 1, calculationState("u1", 0.5));
  assert.deepEqual(tick2.map((event) => event[2]), [
    "preprocessor:run",
    "action:finish",
    "action:run",
    "processor:destruct",
  ]);
  assert.equal(tick2[3][3], "auto:$.objects.unit.processors[1]");
  assert.equal(tick2[3][4], null);

  const tick3 = evaluator.evaluateTick({
    objects: [],
  }, 3, 1, new Map());
  assert.deepEqual(tick3, [
    [3, null, "preprocessor:run", "terrain", null],
    [3, "u1", "object:remove", null, null],
  ]);
});

test("processor graph supports only declared event opcodes and rejects invalid inputs", () => {
  assert.deepEqual(PROCESSOR_EVENT_OPS, [
    "action:finish",
    "action:run",
    "object:alpha",
    "object:create",
    "object:remove",
    "preprocessor:run",
    "processor:destruct",
    "processor:run",
  ]);
  const evaluator = new RendererProcessorEvaluator({
    metadata: { objects: { unit: { processors: [] } } },
    getRandomState: () => 0,
    setRandomState() {},
  });
  assert.throws(
    () => evaluator.evaluateTick({ objects: [] }, -1, 1, new Map()),
    /nonnegative safe integer/,
  );
  assert.throws(
    () => evaluator.evaluateTick({ objects: [] }, 0, 1, {}),
    /must be a Map/,
  );
  assert.throws(() => new RendererProcessorEvaluator({
    metadata: {
      objects: {
        unit: {
          processors: [{ type: "sprite", when: { $random: 1 } }],
        },
      },
    },
  }), /cannot depend on \$random/);

  const random = createRendererRandom(123);
  const hiddenRandom = random;
  const randomPredicate = new RendererProcessorEvaluator({
    metadata: {
      objects: {
        unit: {
          processors: [{
            type: "container",
            when: () => hiddenRandom() > 0,
          }],
        },
      },
    },
    getRandomState: random.getState,
    setRandomState: random.setState,
  });
  assert.throws(() => randomPredicate.evaluateTick({
    objects: [{ _id: "one", type: "unit" }],
  }, 0, 1, calculationState("one", 1)), /consumed hidden randomness/);
  assert.equal(random.getState(), 123);
  assert.throws(() => new RendererProcessorEvaluator({
    metadata: { objects: { unit: { processors: [] } } },
  }), /getRandomState and setRandomState are required/);
});

test("fused graph shares scope and preserves official calculation/action ordering", () => {
  const graph = new RendererGraphEvaluator({
    metadata: {
      objects: {
        unit: {
          calculations: [{
            id: "install",
            func: ({ scope }) => {
              scope.sharedNode = {};
              return true;
            },
          }],
          actions: [{
            id: "targeted",
            targetId: "sharedNode",
            actions: [],
          }],
          processors: [],
        },
      },
    },
    getRandomState: () => 0,
    setRandomState() {},
  });
  const result = graph.evaluateTick({
    objects: [{ _id: "one", type: "unit" }],
  }, 0, 1);
  assert.deepEqual(result.events.map((event) => event[2]), [
    "object:create",
    "action:run",
  ]);
  assert.equal(result.calculations.get("one").install, true);
});

test("rerunning an object processor removes its old node before shouldCreate", () => {
  const graph = new RendererGraphEvaluator({
    metadata: {
      objects: {
        unit: {
          actions: [{
            id: "targeted",
            targetId: "body",
            props: ["value"],
            actions: [],
          }],
          processors: [{
            id: "body-processor",
            type: "sprite",
            props: ["enabled"],
            payload: {
              id: "body",
              texture: "unit",
              shouldCreate: { $state: "enabled" },
            },
          }],
        },
      },
    },
    getRandomState: () => 0,
    setRandomState() {},
  });
  graph.evaluateTick({
    objects: [{ _id: "one", type: "unit", enabled: true, value: 0 }],
  }, 0, 1);
  const removalTick = graph.evaluateTick({
    objects: [{ _id: "one", type: "unit", enabled: false, value: 1 }],
  }, 1, 1);
  assert.deepEqual(removalTick.events.map((event) => event[2]), [
    "action:run",
    "processor:run",
  ]);
  const afterRemoval = graph.evaluateTick({
    objects: [{ _id: "one", type: "unit", enabled: false, value: 2 }],
  }, 2, 1);
  assert.equal(
    afterRemoval.events.some((event) => event[2] === "action:run"),
    false,
  );
});

test("explicit null processor ids address the JavaScript null scope key", () => {
  const graph = new RendererGraphEvaluator({
    metadata: {
      objects: {
        unit: {
          actions: [{
            id: "targeted",
            targetId: "null",
            props: ["value"],
            actions: [],
          }],
          processors: [{
            id: "body",
            type: "sprite",
            payload: {
              id: null,
              texture: "unit",
            },
          }],
        },
      },
    },
    getRandomState: () => 0,
    setRandomState() {},
  });
  graph.evaluateTick({
    objects: [{ _id: "one", type: "unit", value: 0 }],
  }, 0, 1);
  const second = graph.evaluateTick({
    objects: [{ _id: "one", type: "unit", value: 1 }],
  }, 1, 1);
  assert.equal(
    second.events.some((event) => event[2] === "action:run"),
    true,
  );
});

test("resourceCircle early returns retain their targetable scope node", () => {
  const graph = new RendererGraphEvaluator({
    metadata: {
      objects: {
        unit: {
          actions: [{
            id: "targeted",
            targetId: "resource",
            props: ["value"],
            actions: [],
          }],
          processors: [{
            id: "resource",
            type: "resourceCircle",
          }],
        },
      },
    },
    getRandomState: () => 0,
    setRandomState() {},
  });
  graph.evaluateTick({
    objects: [{ _id: "one", type: "unit", energy: 10, value: 0 }],
  }, 0, 1);
  graph.evaluateTick({
    objects: [{ _id: "one", type: "unit", energy: 10, value: 1 }],
  }, 1, 1);
  const third = graph.evaluateTick({
    objects: [{ _id: "one", type: "unit", energy: 10, value: 2 }],
  }, 2, 1);
  assert.equal(
    third.events.some(
      (event) => event[2] === "action:run"
        && event[3] === "auto:$.objects.unit.actions[0]",
    ),
    true,
  );
});

test("siteProgress shares strict oldProgress across an entity scope", () => {
  const evaluator = new RendererProcessorEvaluator({
    metadata: {
      objects: {
        site: {
          processors: [
            {
              id: "first",
              type: "siteProgress",
              props: ["progress"],
              payload: { progress: { $state: "progress" } },
            },
            {
              id: "second",
              type: "siteProgress",
              props: ["progress"],
              payload: { progress: { $state: "progress" } },
            },
          ],
        },
      },
    },
    getRandomState: () => 0,
    setRandomState() {},
  });

  evaluator.evaluateTick({
    objects: [{ _id: "one", type: "site", progress: 1 }],
  }, 0, 1, calculationState("one", 1));
  const record = evaluator.records.get("one");
  assert.deepEqual(
    [...record.processors.values()].map(({ ownsNode }) => ownsNode),
    [true, false],
  );

  evaluator.evaluateTick({
    objects: [{ _id: "one", type: "site", progress: 2 }],
  }, 1, 1, calculationState("one", 1));
  assert.deepEqual(
    [...record.processors.values()].map(({ ownsNode }) => ownsNode),
    [true, false],
  );
});

test("duplicate processor scope IDs retain distinct lifecycle definitions", () => {
  const evaluator = new RendererProcessorEvaluator({
    metadata: {
      objects: {
        unit: {
          processors: [
            { id: "flare", type: "sprite", payload: { texture: "one" } },
            { id: "flare", type: "sprite", payload: { texture: "two" } },
          ],
        },
      },
    },
    getRandomState: () => 0,
    setRandomState() {},
  });
  const events = evaluator.evaluateTick({
    objects: [{ _id: "one", type: "unit" }],
  }, 0, 1, calculationState("one", 1));
  assert.deepEqual(
    events.filter((event) => event[2] === "processor:run").map((event) => event[3]),
    [
      "auto:$.objects.unit.processors[0]",
      "auto:$.objects.unit.processors[1]",
    ],
  );
});

test("duplicate once processor scope IDs preserve official shared lifecycle", () => {
  const evaluator = new RendererProcessorEvaluator({
    metadata: {
      objects: {
        tower: {
          processors: [
            {
              id: "flare",
              type: "sprite",
              when: ({ state }) => state.effect,
              payload: { texture: "effect-flare" },
            },
            {
              id: "flare",
              once: true,
              type: "sprite",
              payload: { texture: "shot-flare" },
            },
          ],
        },
      },
    },
    getRandomState: () => 0,
    setRandomState() {},
  });
  const evaluate = (tick, effect) => evaluator.evaluateTick({
    objects: [{ _id: "tower", type: "tower", effect }],
  }, tick, 1, calculationState("tower", 1))
    .filter((event) => event[2].startsWith("processor:"))
    .map((event) => [event[2], event[3]]);

  assert.deepEqual(evaluate(0, false), [
    ["processor:destruct", "auto:$.objects.tower.processors[0]"],
    ["processor:run", "auto:$.objects.tower.processors[1]"],
  ]);
  assert.deepEqual(evaluate(1, true), [
    ["processor:run", "auto:$.objects.tower.processors[0]"],
  ]);
  assert.deepEqual(evaluate(2, false), [
    ["processor:destruct", "auto:$.objects.tower.processors[0]"],
    ["processor:run", "auto:$.objects.tower.processors[1]"],
  ]);
});
