"use strict";

const assert = require("node:assert/strict");
const test = require("node:test");

const {
  Rational,
  calculateFrameCount,
  createTimelineEvents,
  runVirtualTimeline,
} = require("../virtual-timeline");

function events(options) {
  return [...createTimelineEvents(options)];
}

test("frame zero is rendered at time zero after applying tick zero", () => {
  const result = events({
    frameCount: 1,
    framesPerSecond: 30,
    ticksPerSecond: 3.75,
    substepsPerSecond: 120,
    totalTicks: 0,
  });

  assert.deepEqual(result.map(({ type }) => type), ["applyTick", "render"]);
  assert.equal(result[0].tick, 0n);
  assert.equal(result[0].time.toString(), "0");
  assert.equal(result[1].frame, 0);
  assert.equal(result[1].timestampUs, 0n);
});

test("tick one is applied at time zero after the initial frame", () => {
  const result = events({
    frameCount: 3,
    framesPerSecond: 2,
    ticksPerSecond: 1,
    substepsPerSecond: 4,
    totalTicks: 1,
  });

  assert.deepEqual(
    result.map((event) => [
      event.type,
      event.time?.toString() ?? `${event.from}-${event.to}`,
      event.tick,
    ]),
    [
      ["applyTick", "0", 0n],
      ["render", "0", 0n],
      ["applyTick", "0", 1n],
      ["advance", "0-1/4", 1n],
      ["advance", "1/4-1/2", 1n],
      ["render", "1/2", 1n],
      ["advance", "1/2-3/4", 1n],
      ["advance", "3/4-1", 1n],
      ["render", "1", 1n],
    ],
  );
});

test("a boundary applies the next target before a coincident render", () => {
  const result = events({
    frameCount: 3,
    framesPerSecond: 1,
    ticksPerSecond: 1,
    substepsPerSecond: 4,
    totalTicks: 2,
  });

  assert.deepEqual(
    result.filter(({ type }) => type === "applyTick").map(({ tick, time }) => [tick, time.toString()]),
    [[0n, "0"], [1n, "0"], [2n, "1"]],
  );
  const atFirstBoundary = result.filter((event) => event.time?.equals(1));
  assert.deepEqual(atFirstBoundary.map(({ type, tick }) => [type, tick]), [
    ["applyTick", 2n],
    ["render", 2n],
  ]);
});

test("the endpoint completes the final tick without applying another", () => {
  const result = events({
    framesPerSecond: 2,
    ticksPerSecond: 2,
    substepsPerSecond: 8,
    totalTicks: 3,
  });
  const applies = result.filter(({ type }) => type === "applyTick");
  const last = result.at(-1);

  assert.deepEqual(applies.map(({ tick }) => tick), [0n, 1n, 2n, 3n]);
  assert.equal(last.type, "render");
  assert.equal(last.time.toString(), "3/2");
  assert.equal(last.tick, 3n);
});

test("multiple transition targets cannot be skipped between video frames", () => {
  const result = events({
    framesPerSecond: 1,
    ticksPerSecond: 4,
    substepsPerSecond: 2,
    totalTicks: 4,
  });

  assert.deepEqual(
    result.filter(({ type }) => type === "applyTick").map(({ tick, time }) => [tick, time.toString()]),
    [[0n, "0"], [1n, "0"], [2n, "1/4"], [3n, "1/2"], [4n, "3/4"]],
  );
  assert.equal(result.at(-1).type, "render");
  assert.equal(result.at(-1).time.toString(), "1");
  assert.equal(result.at(-1).tick, 4n);
});

test("fractional frame rates use absolute no-drift timestamps", () => {
  const frameCount = 30_001;
  const result = events({
    frameCount,
    framesPerSecond: "30000/1001",
    ticksPerSecond: "15/4",
    substepsPerSecond: 120,
    totalTicks: 4_000,
  });
  const renders = result.filter(({ type }) => type === "render");
  const last = renders.at(-1);

  assert.equal(last.time.toString(), "1001");
  assert.equal(last.timestampUs, 1_001_000_000n);
  assert.equal(renders[1].timestampUs, 33_367n);
  assert.equal(renders[2].timestampUs, 66_733n);
  assert.equal(
    renders.reduce((sum, frame) => sum + frame.durationUs, 0n),
    1_001_033_367n,
  );
});

test("arbitrary rational tick rates map exact tick boundaries", () => {
  const result = events({
    frameCount: 5,
    framesPerSecond: 4,
    ticksPerSecond: "7/3",
    substepsPerSecond: "11/2",
    totalTicks: 3,
  });

  assert.deepEqual(
    result.filter(({ type }) => type === "applyTick").map(({ tick, time }) => [tick, time.toString()]),
    [[0n, "0"], [1n, "0"], [2n, "3/7"], [3n, "6/7"]],
  );
});

test("frame count includes an exact endpoint on and off the frame grid", () => {
  assert.equal(calculateFrameCount({
    totalTicks: 2_000,
    framesPerSecond: 30,
    ticksPerSecond: "15/4",
  }), 16_001);

  const options = {
    totalTicks: 1,
    framesPerSecond: 2,
    ticksPerSecond: 3,
    substepsPerSecond: 12,
  };
  assert.equal(calculateFrameCount(options), 2);
  const renders = events(options).filter(({ type }) => type === "render");
  assert.deepEqual(renders.map(({ time }) => time.toString()), ["0", "1/3"]);
  assert.equal(renders.at(-1).durationUs, 500_000n);
});

test("an off-grid endpoint shortens the preceding frame without overlap", () => {
  const renders = [...createTimelineEvents({
    totalTicks: 1,
    framesPerSecond: 2,
    ticksPerSecond: 3,
    substepsPerSecond: 6,
  })].filter((event) => event.type === "render");

  assert.deepEqual(renders.map((event) => [event.timestampUs, event.durationUs]), [
    [0n, 333333n],
    [333333n, 500000n],
  ]);
});

test("hooks run sequentially in deterministic event order", async () => {
  const calls = [];
  let active = false;
  const hook = (event) => {
    assert.equal(active, false);
    active = true;
    return Promise.resolve().then(() => {
      calls.push(event.type);
      active = false;
    });
  };

  await runVirtualTimeline(
    {
      frameCount: 2,
      framesPerSecond: 2,
      ticksPerSecond: 3,
      substepsPerSecond: 4,
      totalTicks: 1,
    },
    { applyTick: hook, advance: hook, render: hook },
  );

  assert.deepEqual(calls, [
    "applyTick",
    "render",
    "applyTick",
    "advance",
    "advance",
    "render",
  ]);
});

test("Rational parses decimals, exponents, and explicit fractions exactly", () => {
  assert.equal(Rational.from(3.75).toString(), "15/4");
  assert.equal(Rational.from("1.25e-2").toString(), "1/80");
  assert.equal(Rational.from("30000/1001").toString(), "30000/1001");
});

test("invalid scheduler settings fail early", () => {
  const base = {
    frameCount: 1,
    framesPerSecond: 30,
    ticksPerSecond: 4,
    substepsPerSecond: 120,
    totalTicks: 1,
  };

  assert.throws(() => events({ ...base, frameCount: 0 }), /frameCount/);
  assert.throws(() => events({ ...base, framesPerSecond: 0 }), /framesPerSecond/);
  assert.throws(() => events({ ...base, ticksPerSecond: -1 }), /ticksPerSecond/);
  assert.throws(() => events({ ...base, substepsPerSecond: Infinity }), /finite/);
  assert.throws(() => events({ ...base, totalTicks: -1 }), /totalTicks/);
  assert.throws(() => events({ ...base, totalTicks: undefined }), /totalTicks/);
});
