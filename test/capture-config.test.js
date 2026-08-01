"use strict";

const assert = require("node:assert/strict");
const path = require("node:path");
const test = require("node:test");

const {
  MAX_CAPTURE_DIMENSION,
  resolveCaptureConfig,
} = require("../capture-config");
const { resolveCaptureTransport } = require("../capture-transport");
const { loadVisualStates } = require("../capture-board-runtime");

test("resolveCaptureConfig freezes option/param/env precedence with defaults", () => {
  const config = resolveCaptureConfig({
    options: { totalTicks: 12 },
    params: new URLSearchParams("capture-width=640&capture-fps=24"),
    env: { SCREEPS_ARENA_BOARD_CAPTURE_BITRATE: "12000000" },
    canvasHints: { width: 100, height: 200 },
  });
  assert.equal(Object.isFrozen(config), true);
  assert.equal(config.width, 640);
  assert.equal(config.height, 200);
  assert.equal(config.fps, 24);
  assert.equal(config.framesPerTick, 8);
  assert.equal(config.ticksPerSecond, 24 / 8);
  assert.equal(config.bitrate, 12_000_000);
  assert.equal(config.totalTicks, 12);
  assert.equal(config.compileReplayIR, false);
  assert.equal(config.boardZoom, "auto");
  assert.equal(config.boardPadding, 32);
});

test("resolveCaptureConfig rejects oversized dimensions and bad totals", () => {
  assert.throws(
    () => resolveCaptureConfig({
      options: { width: MAX_CAPTURE_DIMENSION + 1, height: 16, totalTicks: 0 },
    }),
    /width must be <=/,
  );
  assert.throws(
    () => resolveCaptureConfig({
      options: { width: 16, height: 16 },
    }),
    /totalTicks must be a nonnegative/,
  );
  assert.throws(
    () => resolveCaptureConfig({
      options: { width: 16, height: 16, totalTicks: -1 },
    }),
    /totalTicks must be a nonnegative/,
  );
});

test("resolveCaptureConfig treats replay-ir truthy strings as enabled", () => {
  for (const value of ["1", "true", "TRUE", true, 1]) {
    const config = resolveCaptureConfig({
      options: { width: 32, height: 32, totalTicks: 0, compileReplayIR: value },
    });
    assert.equal(config.compileReplayIR, true, String(value));
  }
  const disabled = resolveCaptureConfig({
    options: { width: 32, height: 32, totalTicks: 0, compileReplayIR: "0" },
  });
  assert.equal(disabled.compileReplayIR, false);
});

test("resolveCaptureTransport never falls back to shared /tmp paths", () => {
  const none = resolveCaptureTransport({}, new URLSearchParams(), {});
  assert.equal(none.captureId, null);
  assert.equal(none.fifoPath, null);
  assert.equal(none.errorFile, null);
  assert.equal(none.doneFile, null);
  assert.equal(none.debugFile, null);

  const poisonedEnv = resolveCaptureTransport(
    {},
    new URLSearchParams(),
    {
      SCREEPS_ARENA_BOARD_CAPTURE_ERROR: "/tmp/screeps-arena-capture-error",
      SCREEPS_ARENA_BOARD_CAPTURE_FIFO: "/tmp/evil.fifo",
    },
  );
  assert.equal(poisonedEnv.errorFile, null);
  assert.equal(poisonedEnv.fifoPath, null);

  const explicit = resolveCaptureTransport(
    { errorFile: "/var/tmp/safe.error", fifoPath: "/var/tmp/safe.fifo" },
    new URLSearchParams(),
    { SCREEPS_ARENA_BOARD_CAPTURE_ERROR: "/tmp/ignored" },
  );
  assert.equal(explicit.errorFile, "/var/tmp/safe.error");
  assert.equal(explicit.fifoPath, "/var/tmp/safe.fifo");
});

test("resolveCaptureTransport uses private cache paths for valid capture-id", () => {
  const home = "/home/example";
  const transport = resolveCaptureTransport(
    {},
    new URLSearchParams("capture-id=capture-42"),
    { HOME: home },
  );
  const root = path.join(home, ".cache", "screeps-arena-videoizer");
  assert.equal(transport.captureId, "capture-42");
  assert.equal(transport.fifoPath, path.join(root, "capture-42.fifo"));
  assert.equal(transport.errorFile, path.join(root, "capture-42.error"));
  assert.equal(transport.doneFile, path.join(root, "capture-42.done"));

  const outsideEnv = resolveCaptureTransport(
    {},
    new URLSearchParams("capture-id=capture-42"),
    {
      HOME: home,
      SCREEPS_ARENA_BOARD_CAPTURE_ERROR: "/tmp/outside.error",
    },
  );
  assert.equal(outsideEnv.errorFile, path.join(root, "capture-42.error"));

  const insideEnv = resolveCaptureTransport(
    {},
    new URLSearchParams("capture-id=capture-42"),
    {
      HOME: home,
      SCREEPS_ARENA_BOARD_CAPTURE_ERROR: path.join(root, "custom.error"),
    },
  );
  assert.equal(insideEnv.errorFile, path.join(root, "custom.error"));
});

test("loadVisualStates keeps endpoints empty and loads intermediate ticks once", async () => {
  const calls = [];
  const service = {
    async getTick(tick) {
      calls.push(tick);
      return [{ tick }];
    },
  };
  const states = await loadVisualStates(service, 3, 2, 1_000);
  assert.deepEqual([...states.keys()], [0, 1, 2, 3]);
  assert.deepEqual(states.get(0), []);
  assert.deepEqual(states.get(3), []);
  assert.deepEqual(states.get(1), [{ tick: 1 }]);
  assert.deepEqual(states.get(2), [{ tick: 2 }]);
  assert.deepEqual(calls.sort((a, b) => a - b), [1, 2]);
});

test("loadVisualStates times out hung getTick calls", async () => {
  const service = {
    getTick() {
      return new Promise(() => {});
    },
  };
  await assert.rejects(
    loadVisualStates(service, 2, 1, 20),
    /visual getTick\(1\) timed out/,
  );
});
