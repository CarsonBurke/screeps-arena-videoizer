"use strict";

const assert = require("node:assert/strict");
const test = require("node:test");

const {
  SUPPORTED_EXPRESSION_OPERATORS,
  createResolvedRendererAction,
  evaluateRendererExpression,
  resolveRendererActionSpecification,
  resolvePath,
} = require("../renderer-expressions");

test("expression support exactly covers the retained official arena inventory", () => {
  assert.deepEqual(SUPPORTED_EXPRESSION_OPERATORS, [
    "$calc",
    "$idx",
    "$processorParam",
    "$random",
    "$rel",
    "$state",
  ]);
});

test("resolvePath matches renderer dot, bracket, root, and falsy traversal behavior", () => {
  const value = { creep: { body: [{ hits: 100 }] }, zero: 0 };
  assert.equal(resolvePath(value, "creep.body[0].hits"), 100);
  assert.equal(resolvePath(value, "^"), value);
  assert.equal(resolvePath(value, "missing"), undefined);
  assert.equal(resolvePath(value, "zero.missing"), 0);
});

test("state, calc, relative, and processor expressions apply defaults and coefficients", () => {
  const params = {
    calcs: { energy: 12 },
    state: { x: 4, label: "creep" },
    target: { alpha: 0.25 },
    renderData: { alpha: 0.75 },
    processor: { scale: 3 },
  };
  assert.equal(evaluateRendererExpression({ $calc: "energy", koef: 2 }, params), 24);
  assert.equal(evaluateRendererExpression({ $state: "x", koef: 10 }, params), 40);
  assert.equal(evaluateRendererExpression({ $state: "label", koef: 10 }, params), "creep");
  assert.equal(evaluateRendererExpression({ $state: "missing", default: 7 }, params), 7);
  assert.equal(evaluateRendererExpression({ $rel: "alpha" }, params), 0.25);
  assert.equal(
    evaluateRendererExpression({ $processorParam: "processor.scale" }, params),
    3,
  );
  assert.equal(evaluateRendererExpression({
    $idx: [
      { attack: 0xf73381, heal: 0x56ce9e },
      { $state: "bodyPartType" },
    ],
  }, { state: { bodyPartType: "heal" } }), 0x56ce9e);
  assert.equal(evaluateRendererExpression({ $idx: [["A", "B"], 1] }, {}), "B");
  assert.equal(evaluateRendererExpression({ $idx: [["A", "B"], "01"] }, {}), undefined);
  assert.equal(evaluateRendererExpression({ $idx: [["A", "B"], "length"] }, {}), 2);
  assert.equal(evaluateRendererExpression({ $idx: ["AB", 1] }, {}), "B");
  assert.equal(evaluateRendererExpression({ $idx: [null, "missing"] }, {}), undefined);
  assert.throws(
    () => evaluateRendererExpression({ $idx: [{}] }, {}),
    /\$idx expects \[target, key]/,
  );
});

test("random and nested object expressions use the injected deterministic stream", () => {
  let calls = 0;
  const result = evaluateRendererExpression({
    delay: { $random: 10 },
    nested: [{ $state: "x" }],
  }, { state: { x: 5 } }, () => {
    calls++;
    return 0.125;
  });
  assert.deepEqual(result, { delay: 1.25, nested: [5] });
  assert.equal(calls, 1);
});

test("resolved action creation feeds official expressions into the action core", () => {
  const action = createResolvedRendererAction({
    action: "AlphaTo",
    params: [{ $calc: "alpha" }, 1],
  }, { calcs: { alpha: 0.8 } });
  const target = { alpha: 0 };
  assert.equal(action.update(target, 500), false);
  assert.equal(target.alpha, 0.4);
  assert.equal(action.update(target, 500), true);
  assert.equal(target.alpha, 0.8);
});

test("action specifications can be resolved without losing nested action structure", () => {
  assert.deepEqual(resolveRendererActionSpecification({
    action: "Sequence",
    params: [[
      { action: "DelayTime", params: [{ $state: "delay" }] },
      {
        action: "AlphaTo",
        params: [{ nested: { $calc: "alpha" } }, { $random: 2 }],
      },
    ]],
  }, {
    state: { delay: 0.25 },
    calcs: { alpha: 0.8 },
  }, () => 0.5), {
    action: "Sequence",
    params: [[
      { action: "DelayTime", params: [0.25] },
      { action: "AlphaTo", params: [{ nested: 0.8 }, 1] },
    ]],
  });
});

test("unknown expression operators fail closed at any nesting depth", () => {
  assert.throws(
    () => evaluateRendererExpression({ nested: [{ $future: "x" }] }, {}),
    /unsupported renderer expression \$future/,
  );
});
