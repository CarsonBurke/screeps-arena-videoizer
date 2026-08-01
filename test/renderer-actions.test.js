"use strict";

const assert = require("node:assert/strict");
const test = require("node:test");

const {
  SUPPORTED_ACTION_TYPES,
  advanceRendererAction,
  createRendererAction,
  easing,
} = require("../renderer-actions");

function container(overrides = {}) {
  const value = {
    alpha: 0,
    x: 0,
    y: 0,
    rotation: 0,
    scale: { x: 1, y: 1 },
    tint: 0,
    filters: [{ strength: 0 }],
    ...overrides,
  };
  value.position = {};
  Object.defineProperties(value.position, {
    x: { get: () => value.x, set: (next) => { value.x = next; } },
    y: { get: () => value.y, set: (next) => { value.y = next; } },
  });
  return value;
}

test("action support exactly covers the official arena contract inventory", () => {
  assert.deepEqual(SUPPORTED_ACTION_TYPES, [
    "AlphaTo",
    "DelayTime",
    "Ease",
    "FadeIn",
    "FadeOut",
    "FilterTo",
    "MoveTo",
    "Repeat",
    "RotateBy",
    "RotateTo",
    "ScaleTo",
    "Sequence",
    "Spawn",
    "TintTo",
  ]);
});

test("FadeIn and FadeOut finish without resetting restTime like AlphaTo", () => {
  const fadeTarget = container({ alpha: 0 });
  const fadeIn = createRendererAction({ action: "FadeIn", params: [1] });
  assert.equal(advanceRendererAction(fadeIn, fadeTarget, 250), false);
  assert.equal(fadeTarget.alpha, 0.25);
  assert.equal(advanceRendererAction(fadeIn, fadeTarget, 750), true);
  assert.equal(fadeTarget.alpha, 1);
  // Official FadeIn.finish does not call Action.reset(), so restTime stays spent.
  const restAfterFinish = fadeIn.restTime;
  assert.ok(restAfterFinish <= 0);
  assert.equal(advanceRendererAction(fadeIn, fadeTarget, 100), true);
  assert.equal(fadeTarget.alpha, 1);
  assert.ok(fadeIn.restTime < restAfterFinish);

  const fadeOutTarget = container({ alpha: 1 });
  const fadeOut = createRendererAction({ action: "FadeOut", params: [0.5] });
  assert.equal(advanceRendererAction(fadeOut, fadeOutTarget, 500), true);
  assert.equal(fadeOutTarget.alpha, 0);

  const zeroTarget = container({ alpha: 0.2 });
  const zeroFade = createRendererAction({ action: "FadeIn", params: [0] });
  assert.equal(advanceRendererAction(zeroFade, zeroTarget, 16), true);
  assert.equal(zeroTarget.alpha, 1);

  const alphaTarget = container({ alpha: 0 });
  const alphaTo = createRendererAction({ action: "AlphaTo", params: [1, 1] });
  assert.equal(advanceRendererAction(alphaTo, alphaTarget, 1000), true);
  assert.equal(alphaTarget.alpha, 1);
  // AlphaTo.finish → Action.reset restores restTime for another cycle.
  assert.equal(alphaTo.restTime, 1000);
});

test("zero-duration AlphaTo and pre-update RotateBy.finish match official quirks", () => {
  const alphaTarget = container({ alpha: 0 });
  const alpha = createRendererAction({ action: "AlphaTo", params: [0.2, 0] });
  assert.equal(advanceRendererAction(alpha, alphaTarget, 16), true);
  assert.equal(alphaTarget.alpha, 0.2);

  const rotateTarget = container({ rotation: 1 });
  const rotate = createRendererAction({ action: "RotateBy", params: [1, 1] });
  rotate.finish(rotateTarget);
  // Official RotateBy.finish assigns the unset target (null). Pixi transform
  // math then coerces that effective rotation to zero; the native runtime
  // stores the effective 0.0. Pure JS mirrors the official assignment.
  assert.equal(rotateTarget.rotation, null);

  assert.throws(
    () => advanceRendererAction(alpha, alphaTarget, -1),
    /deltaMs must be a nonnegative finite number/,
  );
});

test("timeable scalar and vector actions match fixed-step endpoint behavior", () => {
  const alphaContainer = container();
  const alpha = createRendererAction({ action: "AlphaTo", params: [1, 1] });
  assert.equal(advanceRendererAction(alpha, alphaContainer, 250), false);
  assert.equal(alphaContainer.alpha, 0.25);
  assert.equal(advanceRendererAction(alpha, alphaContainer, 250), false);
  assert.equal(alphaContainer.alpha, 0.5);
  assert.equal(advanceRendererAction(alpha, alphaContainer, 500), true);
  assert.equal(alphaContainer.alpha, 1);

  const vectorContainer = container();
  const actions = [
    createRendererAction({ action: "MoveTo", params: [8, -4, 1] }),
    createRendererAction({ action: "ScaleTo", params: [3, 5, 1] }),
    createRendererAction({ action: "FilterTo", params: [0, "strength", 2, 1] }),
  ];
  for (const action of actions) assert.equal(advanceRendererAction(action, vectorContainer, 500), false);
  assert.deepEqual(
    { x: vectorContainer.x, y: vectorContainer.y, scale: vectorContainer.scale, filter: vectorContainer.filters[0] },
    { x: 4, y: -2, scale: { x: 2, y: 3 }, filter: { strength: 1 } },
  );
  for (const action of actions) assert.equal(advanceRendererAction(action, vectorContainer, 500), true);
  assert.deepEqual(
    { x: vectorContainer.x, y: vectorContainer.y, scale: vectorContainer.scale, filter: vectorContainer.filters[0] },
    { x: 8, y: -4, scale: { x: 3, y: 5 }, filter: { strength: 2 } },
  );
});

test("TintTo floors each RGB component on every integration step", () => {
  const target = container({ tint: 0 });
  const action = createRendererAction({ action: "TintTo", params: [0xffffff, 1] });
  const colors = [];
  for (let step = 0; step < 4; step++) {
    advanceRendererAction(action, target, 250);
    colors.push(target.tint);
  }
  assert.deepEqual(colors, [0x3f3f3f, 0x7f7f7f, 0xbfbfbf, 0xffffff]);
});

test("RotateTo takes the official shortest path and mutates its normalized target", () => {
  const target = container({ rotation: Math.PI - 0.1 });
  const action = createRendererAction({
    action: "RotateTo",
    params: [-Math.PI + 0.1, 1],
  });
  advanceRendererAction(action, target, 500);
  assert.ok(Math.abs(target.rotation - Math.PI) < 1e-12);
  advanceRendererAction(action, target, 500);
  assert.ok(Math.abs(target.rotation - (Math.PI + 0.1)) < 1e-12);
  action.reset();
  assert.ok(action.rotation > Math.PI);
});

test("Sequence discards remainder and reports completion one update later", () => {
  const target = container();
  const action = createRendererAction({
    action: "Sequence",
    params: [[
      { action: "DelayTime", params: [0.01] },
      { action: "AlphaTo", params: [1, 0.01] },
    ]],
  });
  assert.equal(advanceRendererAction(action, target, 10), false);
  assert.equal(target.alpha, 0);
  assert.equal(advanceRendererAction(action, target, 10), false);
  assert.equal(target.alpha, 1);
  assert.equal(advanceRendererAction(action, target, 10), true);
});

test("Repeat and Spawn preserve official child reset and completion timing", () => {
  const repeat = createRendererAction({
    action: "Repeat",
    params: [{ action: "DelayTime", params: [0.01] }, 2],
  });
  const target = container();
  assert.equal(advanceRendererAction(repeat, target, 10), false);
  assert.equal(advanceRendererAction(repeat, target, 10), true);

  const spawn = createRendererAction({
    action: "Spawn",
    params: [[
      { action: "AlphaTo", params: [1, 0.01] },
      { action: "MoveTo", params: [2, 3, 0.01] },
    ]],
  });
  assert.equal(advanceRendererAction(spawn, target, 10), true);
  assert.deepEqual({ alpha: target.alpha, x: target.x, y: target.y }, { alpha: 1, x: 2, y: 3 });
});

test("Ease transforms deltas using the official easing table", () => {
  assert.equal(easing.LINEAR(0.25), 0.25);
  assert.equal(easing.EASE_IN_OUT_QUAD(0.25), 0.125);
  assert.equal(easing.EASE_IN_OUT_QUAD(0.75), 0.875);

  const target = container();
  const action = createRendererAction({
    action: "Ease",
    params: [
      { action: "AlphaTo", params: [1, 1] },
      "EASE_IN_QUAD",
    ],
  });
  assert.equal(advanceRendererAction(action, target, 500), false);
  assert.equal(target.alpha, 0.25);
  assert.equal(advanceRendererAction(action, target, 500), true);
  assert.equal(target.alpha, 1);
});

test("unresolved expressions and unknown actions fail closed", () => {
  assert.throws(
    () => createRendererAction({ action: "AlphaTo", params: [{ $calc: "alpha" }, 1] }),
    /must be resolved/,
  );
  assert.throws(
    () => createRendererAction({
      action: "AlphaTo",
      params: [{ nested: { $calc: "alpha" } }, 1],
    }),
    /must be resolved/,
  );
  assert.throws(
    () => createRendererAction({ action: "SkewTo", params: [1, 1, 1] }),
    /unsupported renderer action/,
  );
});
