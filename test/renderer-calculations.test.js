"use strict";

const assert = require("node:assert/strict");
const test = require("node:test");

const {
  RendererCalculationEvaluator,
  evaluateRendererCalculations,
  rendererEqual,
} = require("../renderer-calculations");
const { createRendererRandom } = require("../renderer-random");

function metadata(calculations, data = {}) {
  return {
    objects: {
      _all: {
        calculations: [{
          id: "owner",
          props: ["user"],
          func: ({ state, stateExtra }) => state.user === stateExtra.gameData.player,
        }],
        data: { common: { $state: "x" } },
      },
      creep: {
        calculations,
        data,
      },
    },
  };
}

test("renderer equality matches Lodash semantics for replay calculation values", () => {
  assert.equal(rendererEqual(0, -0), true);
  assert.equal(rendererEqual(NaN, NaN), true);
  assert.equal(rendererEqual({ a: [1, { b: 2 }] }, { a: [1, { b: 2 }] }), true);
  assert.equal(rendererEqual({ a: undefined }, {}), false);
  assert.equal(rendererEqual([1], [1, undefined]), false);
});

test("calculations retain prior values and observe earlier calculations in the same tick", () => {
  let firstRuns = 0;
  const evaluator = new RendererCalculationEvaluator({
    metadata: metadata([
      {
        id: "position",
        props: ["x"],
        func: ({ state, firstRun }) => {
          if (firstRun) firstRuns++;
          return state.x * 2;
        },
      },
      {
        id: "dependent",
        props: ["position"],
        func: ({ calcs }) => calcs.position + 1,
      },
      {
        id: "always",
        func: ({ stateExtra }) => stateExtra.gameTime,
      },
    ]),
    world: { options: { CELL_SIZE: 100, gameData: { player: "u1" } } },
    random: () => 0.5,
  });

  const first = evaluator.evaluateTick({
    gameTime: 10,
    player: "u1",
    objects: [{ _id: "one", type: "creep", user: "u1", x: 2 }],
  }, 1);
  assert.deepEqual(first.get("one"), {
    owner: true,
    position: 4,
    dependent: 5,
    always: 10,
  });

  const second = evaluator.evaluateTick({
    gameTime: 11,
    player: "u1",
    objects: [{ _id: "one", type: "creep", user: "u1", x: 2 }],
  }, 1);
  assert.deepEqual(second.get("one"), {
    owner: true,
    position: 4,
    dependent: 5,
    always: 11,
  });

  const third = evaluator.evaluateTick({
    gameTime: 12,
    player: "u1",
    objects: [{ _id: "one", type: "creep", user: "u1", x: 3 }],
  }, 1);
  assert.deepEqual(third.get("one"), {
    owner: true,
    position: 6,
    dependent: 7,
    always: 12,
  });
  assert.equal(firstRuns, 1);
});

test("when, path, payload, data, and deterministic expressions match renderer evaluation", () => {
  const evaluator = new RendererCalculationEvaluator({
    metadata: metadata([
      {
        id: "nested",
        path: "store.energy",
        payload: { multiplier: 3 },
        props: ["store"],
        when: ({ state }) => state.enabled,
        func: ({ state, payload, rootContainer }) => (
          state * payload.multiplier + rootContainer.offset + rootContainer.common
        ),
      },
      {
        id: "random",
        props: ["x"],
        func: { $random: 10 },
      },
    ], {
      offset: { $state: "y" },
    }),
    random: () => 0.25,
  });
  const result = evaluator.evaluateTick({
    player: "u1",
    objects: [{
      _id: "one",
      type: "creep",
      user: "u2",
      x: 4,
      y: 5,
      enabled: true,
      store: { energy: 7 },
    }],
  }, 0.5);
  assert.deepEqual(result.get("one"), {
    owner: false,
    nested: 30,
    random: 2.5,
  });
});

test("absence resets calculation lifecycle and source state is not mutated", () => {
  const states = [
    {
      objects: [{ _id: "one", type: "creep", user: "u1", x: 1 }],
    },
    { objects: [] },
    {
      objects: [{ _id: "one", type: "creep", user: "u1", x: 1 }],
    },
  ];
  let runs = 0;
  const results = evaluateRendererCalculations({
    states,
    tickDuration: 1,
    metadata: metadata([{
      id: "run",
      props: [],
      func: ({ stateExtra }) => {
        runs++;
        stateExtra.memo = true;
        return runs;
      },
    }]),
  });
  assert.equal(results[0].get("one").run, 1);
  assert.equal(results[1].size, 0);
  assert.equal(results[2].get("one").run, 2);
  assert.equal(states[0].memo, undefined);
  assert.equal(states[2].memo, undefined);
});

test("calculation execution follows renderer creation order after source reordering", () => {
  const calls = [];
  const evaluator = new RendererCalculationEvaluator({
    metadata: metadata([{
      id: "order",
      func: ({ state }) => {
        calls.push(state._id);
        return calls.length;
      },
    }]),
  });
  evaluator.evaluateTick({
    objects: [
      { _id: "a", type: "creep" },
      { _id: "b", type: "creep" },
    ],
  }, 1);
  const reordered = evaluator.evaluateTick({
    objects: [
      { _id: "b", type: "creep" },
      { _id: "a", type: "creep" },
    ],
  }, 1);
  assert.deepEqual(calls, ["a", "b", "a", "b"]);
  assert.equal(reordered.get("a").order, 3);
  assert.equal(reordered.get("b").order, 4);
});

test("integer-like object IDs follow the renderer's Object.values ordering", () => {
  const calls = [];
  const evaluator = new RendererCalculationEvaluator({
    metadata: metadata([{
      id: "order",
      func: ({ state }) => {
        calls.push(state._id);
        return calls.length;
      },
    }]),
  });
  const result = evaluator.evaluateTick({
    objects: [
      { _id: "10", type: "creep" },
      { _id: "2", type: "creep" },
      { _id: "named", type: "creep" },
    ],
  }, 1);
  assert.deepEqual(calls, ["2", "10", "named"]);
  assert.deepEqual([...result.keys()], ["2", "10", "named"]);
});

test("scene-changing object filters fail closed before calculation tracks diverge", () => {
  const evaluator = new RendererCalculationEvaluator({
    metadata: metadata([]),
    world: {
      options: {
        objectFilter: (objects) => objects.filter(({ type }) => type === "creep"),
      },
    },
  });
  assert.throws(() => evaluator.evaluateTick({
    objects: [
      { _id: "one", type: "creep" },
      { _id: "ignored", type: "unsupported" },
    ],
  }, 1), /requires the full processor compiler/);
});

test("unknown object types, duplicate ids, invalid props, and operators fail closed", () => {
  const evaluator = new RendererCalculationEvaluator({
    metadata: metadata([]),
  });
  assert.throws(() => evaluator.evaluateTick({
    objects: [{ _id: "one", type: "unknown" }],
  }, 1), /does not support object type/);
  assert.throws(() => evaluator.evaluateTick({
    objects: [
      { _id: "one", type: "creep" },
      { _id: "one", type: "creep" },
    ],
  }, 1), /duplicate object identity/);

  const invalidProps = new RendererCalculationEvaluator({
    metadata: metadata([{ id: "bad", props: "x", func: () => true }]),
  });
  invalidProps.evaluateTick({
    objects: [{ _id: "one", type: "creep", x: 1 }],
  }, 1);
  assert.throws(() => invalidProps.evaluateTick({
    objects: [{ _id: "one", type: "creep", x: 2 }],
  }, 1), /props must/);

  const unknownExpression = new RendererCalculationEvaluator({
    metadata: metadata([{ id: "bad", func: { $future: "x" } }]),
  });
  assert.throws(() => unknownExpression.evaluateTick({
    objects: [{ _id: "one", type: "creep" }],
  }, 1), /unsupported renderer expression/);

  const randomFunction = new RendererCalculationEvaluator({
    metadata: metadata([{ id: "bad", func: () => Math.random() }]),
    rejectFunctionRandom: true,
    getRandomState: () => 0,
    setRandomState() {},
  });
  assert.throws(() => randomFunction.evaluateTick({
    objects: [{ _id: "one", type: "creep" }],
  }, 1), /random renderer calculation functions/);

  const randomHelper = () => Math.random();
  const delegatedRandom = new RendererCalculationEvaluator({
    metadata: metadata([{ id: "bad", func: () => randomHelper() }]),
    rejectFunctionRandom: true,
    getRandomState: () => 0,
    setRandomState() {},
  });
  assert.throws(() => delegatedRandom.evaluateTick({
    objects: [{ _id: "one", type: "creep" }],
  }, 1), /random renderer calculations cannot be precomputed/);

  const statefulRandom = createRendererRandom(123);
  const capturedRandom = statefulRandom;
  const capturedRandomEvaluator = new RendererCalculationEvaluator({
    metadata: metadata([{ id: "bad", func: () => capturedRandom() }]),
    rejectFunctionRandom: true,
    getRandomState: statefulRandom.getState,
    setRandomState: statefulRandom.setState,
  });
  assert.throws(() => capturedRandomEvaluator.evaluateTick({
    objects: [{ _id: "one", type: "creep" }],
  }, 1), /captured renderer randomness cannot be precomputed/);
  assert.equal(statefulRandom.getState(), 123);
});
