"use strict";

const assert = require("node:assert/strict");
const test = require("node:test");

const {
  compileStateBatch,
  planFrameBatches,
  planReplayChunkEnds,
  planStateBatches,
  planTemporalWorkUnits,
  preloadReplayChunks,
  prepareStateBatches,
  reconstructStateBatch,
} = require("../replay-batches");

test("replay chunk plans cover the initial and partial final chunks", () => {
  assert.deepEqual(planReplayChunkEnds({ totalTicks: 0 }), [0]);
  assert.deepEqual(planReplayChunkEnds({ totalTicks: 200 }), [0, 100, 200]);
  assert.deepEqual(planReplayChunkEnds({ totalTicks: 205 }), [0, 100, 200, 205]);
  assert.deepEqual(planReplayChunkEnds({ totalTicks: 205, includeInitial: false }), [100, 200, 205]);
});

test("replay chunks preload with bounded request concurrency", async () => {
  let active = 0;
  let peakActive = 0;
  const loaded = await preloadReplayChunks({
    totalTicks: 350,
    concurrency: 2,
    async loadChunk(endTick) {
      active++;
      peakActive = Math.max(peakActive, active);
      await Promise.resolve();
      active--;
      return `chunk-${endTick}`;
    },
  });
  assert.deepEqual([...loaded.keys()], [0, 100, 200, 300, 350]);
  assert.equal(loaded.get(350), "chunk-350");
  assert.equal(peakActive, 2);
});

test("state batches use 50-tick work units with explicit checkpoints", () => {
  assert.deepEqual(planStateBatches({ totalTicks: 120, ticksPerBatch: 50 }), [
    { index: 0, startTick: 0, endTick: 49, checkpointTick: 0, tickCount: 50 },
    { index: 1, startTick: 50, endTick: 99, checkpointTick: 49, tickCount: 50 },
    { index: 2, startTick: 100, endTick: 120, checkpointTick: 99, tickCount: 21 },
  ]);
});

test("compiled state deltas reconstruct spawn, update, removal, and ordering", () => {
  const states = new Map([
    [0, { tick: 0, score: 0, objects: [{ _id: "a", x: 1 }, { _id: "b", x: 2 }] }],
    [1, { tick: 1, score: 3, objects: [{ _id: "b", x: 4 }, { _id: "c", x: 5 }] }],
    [2, { tick: 2, score: 3, objects: [{ _id: "c", x: 5 }, { _id: "b", x: 4 }] }],
  ]);
  const plan = { index: 0, startTick: 0, endTick: 2, checkpointTick: 0, tickCount: 3 };
  const compiled = compileStateBatch(plan, states);
  assert.deepEqual(compiled.transitions[0].removals, ["a"]);
  assert.deepEqual(compiled.transitions[0].upserts, [{ _id: "b", x: 4 }, { _id: "c", x: 5 }]);
  assert.deepEqual(compiled.transitions[1].upserts, []);
  assert.deepEqual([...reconstructStateBatch(compiled).entries()], [...states.entries()]);
});

test("batch object identity matches ReplayIR for null _id and room/type/x/y keys", () => {
  const { replayObjectId } = require("../replay-ir");
  const identity = { _id: null, room: "W0N0", type: "source", x: 5, y: 5 };
  const states = new Map([
    [0, { tick: 0, objects: [identity] }],
    [1, { tick: 1, objects: [{ ...identity, energy: 1 }] }],
  ]);
  const plan = { index: 0, startTick: 0, endTick: 1, checkpointTick: 0, tickCount: 2 };
  const compiled = compileStateBatch(plan, states);
  const expectedId = replayObjectId(identity);
  assert.equal(expectedId, "W0N0:source:5:5");
  assert.deepEqual(compiled.transitions[0].objectOrder, [expectedId]);
  assert.deepEqual(compiled.transitions[0].upserts[0].energy, 1);
  assert.throws(
    () => compileStateBatch(plan, new Map([
      [0, { tick: 0, objects: [{ type: "source", x: 1, y: 2 }] }],
      [1, { tick: 1, objects: [{ type: "source", x: 1, y: 2 }] }],
    ])),
    /missing identity field room/,
  );

  const corrupt = {
    plan,
    checkpoint: states.get(0),
    transitions: [{
      tick: 1,
      upserts: [],
      removals: [],
      objectOrder: ["missing-id"],
      state: { tick: 1 },
    }],
  };
  assert.throws(
    () => reconstructStateBatch(corrupt),
    /missing object missing-id/,
  );
});

test("parallel preparation deduplicates checkpoint loads", async () => {
  const calls = new Map();
  let active = 0;
  let peakActive = 0;
  const prepared = await prepareStateBatches({
    totalTicks: 105,
    ticksPerBatch: 50,
    concurrency: 3,
    async loadState(tick) {
      active++;
      peakActive = Math.max(peakActive, active);
      calls.set(tick, (calls.get(tick) || 0) + 1);
      await Promise.resolve();
      active--;
      return { tick, objects: [{ _id: "static", value: tick % 2 }] };
    },
  });
  assert.equal(prepared.batches.length, 3);
  assert.equal(prepared.statesByTick.size, 106);
  assert.equal(Math.max(...calls.values()), 1);
  assert.equal(peakActive, 3);
});

test("frame microbatches cover every frame exactly once", () => {
  const batches = planFrameBatches({
    totalTicks: 2,
    framesPerSecond: 30,
    ticksPerSecond: 5,
    substepsPerSecond: 60,
    framesPerBatch: 4,
  });
  assert.deepEqual(batches.map((batch) => batch.frames.length), [4, 4, 4, 1]);
  const frames = batches.flatMap((batch) => batch.frames);
  assert.deepEqual(frames.map((frame) => frame.frame), [...Array(13).keys()]);
  assert.equal(frames[0].tick, 0);
  assert.equal(frames[0].progress, 0);
  assert.equal(frames.at(-1).tick, 2);
  assert.equal(frames.at(-1).progress, 1);
});

test("50-tick temporal units own every frame exactly once", () => {
  const units = planTemporalWorkUnits({
    totalTicks: 120,
    ticksPerUnit: 50,
    framesPerSecond: 30,
    ticksPerSecond: 5,
  });
  assert.deepEqual(units, [
    {
      index: 0,
      startBoundaryTick: 0,
      endBoundaryTick: 50,
      frameStart: 0,
      frameEndExclusive: 300,
      frameCount: 300,
      final: false,
      pixiResumable: false,
    },
    {
      index: 1,
      startBoundaryTick: 50,
      endBoundaryTick: 100,
      frameStart: 300,
      frameEndExclusive: 600,
      frameCount: 300,
      final: false,
      pixiResumable: false,
    },
    {
      index: 2,
      startBoundaryTick: 100,
      endBoundaryTick: 120,
      frameStart: 600,
      frameEndExclusive: 721,
      frameCount: 121,
      final: true,
      pixiResumable: false,
    },
  ]);
});

test("fractional temporal-unit boundaries have no gaps or duplicate frames", () => {
  const units = planTemporalWorkUnits({
    totalTicks: 121,
    ticksPerUnit: 50,
    framesPerSecond: "30000/1001",
    ticksPerSecond: "15/4",
  });
  assert.equal(units[0].frameStart, 0);
  for (let index = 1; index < units.length; index++) {
    assert.equal(units[index].frameStart, units[index - 1].frameEndExclusive);
  }
  assert.equal(units.at(-1).frameEndExclusive, 969);
  assert.equal(units.reduce((sum, unit) => sum + unit.frameCount, 0), 969);
  assert.deepEqual(planTemporalWorkUnits({
    totalTicks: 0,
    framesPerSecond: 30,
    ticksPerSecond: 5,
  }).map((unit) => [unit.frameStart, unit.frameEndExclusive]), [[0, 1]]);
});
