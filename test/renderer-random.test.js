"use strict";

const assert = require("node:assert/strict");
const test = require("node:test");

const {
  createRendererRandom,
  hashRendererSeed,
} = require("../renderer-random");

test("renderer random reproduces the capture constructor sequence from any saved state", () => {
  const state = hashRendererSeed("arena-123");
  assert.equal(state, 863351649);
  const random = createRendererRandom(state);
  assert.deepEqual(
    [random(), random(), random()],
    [
      0.6499485343229026,
      0.8805961678735912,
      0.5095910185482353,
    ],
  );
  const checkpoint = random.getState();
  const expected = random();
  random.setState(checkpoint);
  assert.equal(random(), expected);
});

test("mulberry32 seed 123 matches the native RendererRandom golden stream", () => {
  // Must stay bit-identical with native-renderer/src/renderer_random.rs tests.
  const random = createRendererRandom(123);
  assert.deepEqual(
    [random(), random(), random(), random()],
    [
      0.7872516233474016,
      0.1785435655619949,
      0.49531551403924823,
      0.23136196262203157,
    ],
  );
  assert.equal(random.getState(), 3_031_296_079);
});

test("edge seeds wrap as unsigned 32-bit", () => {
  for (const seed of [0, 0xffffffff, 2 ** 32 - 1, -1]) {
    const random = createRendererRandom(seed);
    const first = random();
    assert.ok(first >= 0 && first < 1);
    assert.equal(random.getState() >>> 0, random.getState());
  }
});
