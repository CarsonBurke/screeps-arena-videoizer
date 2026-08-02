"use strict";

const { replayObjectId } = require("./replay-ir");
const { createTimelineEvents, Rational } = require("./virtual-timeline");

function positiveInteger(value, fallback, name) {
  const number = value === undefined || value === null || value === ""
    ? fallback
    : Number(value);
  if (!Number.isSafeInteger(number) || number <= 0) {
    throw new RangeError(`${name} must be a positive safe integer`);
  }
  return number;
}

function nonnegativeInteger(value, name) {
  const number = Number(value);
  if (!Number.isSafeInteger(number) || number < 0) {
    throw new RangeError(`${name} must be a nonnegative safe integer`);
  }
  return number;
}

function planStateBatches(options) {
  if (!options || typeof options !== "object") throw new TypeError("options must be an object");
  const totalTicks = nonnegativeInteger(options.totalTicks, "totalTicks");
  const ticksPerBatch = positiveInteger(options.ticksPerBatch, 50, "ticksPerBatch");
  const batches = [];
  for (let startTick = 0, index = 0; startTick <= totalTicks; startTick += ticksPerBatch, index++) {
    const endTick = Math.min(totalTicks, startTick + ticksPerBatch - 1);
    batches.push(Object.freeze({
      index,
      startTick,
      endTick,
      checkpointTick: Math.max(0, startTick - 1),
      tickCount: endTick - startTick + 1,
    }));
  }
  return Object.freeze(batches);
}

/** Shared with ReplayIR so delta batches and entity tracks cannot diverge. */
function objectId(object) {
  return replayObjectId(object);
}

function objectSignatures(objects) {
  const signatures = new Map();
  for (const object of objects || []) signatures.set(objectId(object), JSON.stringify(object));
  return signatures;
}

function compileStateBatch(plan, states) {
  if (!plan || typeof plan !== "object") throw new TypeError("plan must be an object");
  if (!(states instanceof Map)) throw new TypeError("states must be a Map keyed by tick");
  const checkpoint = states.get(plan.checkpointTick);
  if (!checkpoint || !Array.isArray(checkpoint.objects)) {
    throw new Error(`missing checkpoint state ${plan.checkpointTick}`);
  }

  let previousState = checkpoint;
  let previousSignatures = objectSignatures(checkpoint.objects);
  const transitions = [];
  const firstTransition = plan.startTick === plan.checkpointTick
    ? plan.startTick + 1
    : plan.startTick;
  for (let tick = firstTransition; tick <= plan.endTick; tick++) {
    const state = states.get(tick);
    if (!state || !Array.isArray(state.objects)) throw new Error(`missing state ${tick}`);
    const signatures = objectSignatures(state.objects);
    const previousObjects = new Map(previousState.objects.map((object) => [objectId(object), object]));
    const upserts = [];
    for (const object of state.objects) {
      const id = objectId(object);
      if (signatures.get(id) !== previousSignatures.get(id)) upserts.push(object);
      previousObjects.delete(id);
    }
    transitions.push(Object.freeze({
      tick,
      upserts: Object.freeze(upserts),
      removals: Object.freeze([...previousObjects.keys()]),
      objectOrder: Object.freeze(state.objects.map(objectId)),
      // Global fields are generally small and may influence renderer processors.
      // Preserve them intact instead of guessing a lossy field-level diff.
      state: Object.freeze(Object.assign({}, state, { objects: undefined })),
    }));
    previousState = state;
    previousSignatures = signatures;
  }

  return Object.freeze({
    plan,
    checkpoint,
    transitions: Object.freeze(transitions),
  });
}

function reconstructStateBatch(compiled) {
  if (!compiled || !compiled.checkpoint || !Array.isArray(compiled.transitions)) {
    throw new TypeError("compiled batch is invalid");
  }
  const states = new Map([[compiled.plan.checkpointTick, compiled.checkpoint]]);
  const objects = new Map(compiled.checkpoint.objects.map((object) => [objectId(object), object]));
  for (const transition of compiled.transitions) {
    for (const id of transition.removals) objects.delete(id);
    for (const object of transition.upserts) objects.set(objectId(object), object);
    states.set(transition.tick, Object.freeze(Object.assign({}, transition.state, {
      objects: Object.freeze(transition.objectOrder.map((id) => {
        const object = objects.get(id);
        if (object === undefined) {
          throw new Error(`missing object ${id} while reconstructing tick ${transition.tick}`);
        }
        return object;
      })),
    })));
  }
  return states;
}

async function mapConcurrent(items, concurrency, mapper) {
  const results = new Array(items.length);
  let next = 0;
  async function worker() {
    while (true) {
      const index = next++;
      if (index >= items.length) return;
      results[index] = await mapper(items[index], index);
    }
  }
  await Promise.all(Array.from(
    { length: Math.min(concurrency, items.length) },
    () => worker(),
  ));
  return results;
}

function planReplayChunkEnds(options) {
  if (!options || typeof options !== "object") throw new TypeError("options must be an object");
  const totalTicks = nonnegativeInteger(options.totalTicks, "totalTicks");
  const chunkLength = positiveInteger(options.chunkLength, 100, "chunkLength");
  const includeInitial = options.includeInitial !== false;
  const ends = includeInitial ? [0] : [];
  for (let end = chunkLength; end < totalTicks; end += chunkLength) ends.push(end);
  if (totalTicks > 0 && (ends.length === 0 || ends.at(-1) !== totalTicks)) ends.push(totalTicks);
  return Object.freeze(ends);
}

async function preloadReplayChunks(options) {
  if (!options || typeof options !== "object") throw new TypeError("options must be an object");
  if (typeof options.loadChunk !== "function") throw new TypeError("loadChunk must be a function");
  const concurrency = positiveInteger(options.concurrency, 4, "concurrency");
  const chunkEnds = planReplayChunkEnds(options);
  const loaded = await mapConcurrent(chunkEnds, concurrency, async (endTick) => [
    endTick,
    await options.loadChunk(endTick),
  ]);
  return new Map(loaded);
}

async function prepareStateBatches(options) {
  if (!options || typeof options !== "object") throw new TypeError("options must be an object");
  if (typeof options.loadState !== "function") throw new TypeError("loadState must be a function");
  const plans = planStateBatches(options);
  const concurrency = positiveInteger(options.concurrency, 4, "concurrency");
  const ticks = [...Array(nonnegativeInteger(options.totalTicks, "totalTicks") + 1).keys()];
  const loaded = await mapConcurrent(ticks, concurrency, async (tick) => [
    tick,
    await options.loadState(tick),
  ]);
  const statesByTick = new Map(loaded);
  // These are immutable replay-data checkpoints for the track compiler. They
  // are deliberately not advertised as resumable Pixi scene checkpoints.
  const batches = options.compileDeltas === false
    ? []
    : plans.map((plan) => compileStateBatch(plan, statesByTick));
  return Object.freeze({ plans, batches: Object.freeze(batches), statesByTick });
}

function planFrameBatches(options) {
  if (!options || typeof options !== "object") throw new TypeError("options must be an object");
  const framesPerBatch = positiveInteger(options.framesPerBatch, 16, "framesPerBatch");
  const ticksPerSecond = Rational.from(options.ticksPerSecond, "ticksPerSecond");
  const batches = [];
  let frames = [];
  for (const event of createTimelineEvents(options)) {
    if (event.type !== "render") continue;
    const tick = Number(event.tick);
    let progress = 0;
    if (tick > 0) {
      const transitionStart = new Rational(BigInt(tick - 1)).divide(ticksPerSecond);
      progress = event.time.subtract(transitionStart).multiply(ticksPerSecond).toNumber();
      progress = Math.max(0, Math.min(1, progress));
    }
    frames.push(Object.freeze({
      frame: event.frame,
      tick,
      progress,
      timeSeconds: event.time.toNumber(),
      timestampUs: Number(event.timestampUs),
      durationUs: Number(event.durationUs),
    }));
    if (frames.length === framesPerBatch) {
      batches.push(Object.freeze({ index: batches.length, frames: Object.freeze(frames) }));
      frames = [];
    }
  }
  if (frames.length > 0) batches.push(Object.freeze({ index: batches.length, frames: Object.freeze(frames) }));
  return Object.freeze(batches);
}

function planTemporalWorkUnits(options) {
  if (!options || typeof options !== "object") throw new TypeError("options must be an object");
  const totalTicks = nonnegativeInteger(options.totalTicks, "totalTicks");
  const ticksPerUnit = positiveInteger(options.ticksPerUnit, 50, "ticksPerUnit");
  const framesPerSecond = Rational.from(options.framesPerSecond, "framesPerSecond");
  const ticksPerSecond = Rational.from(options.ticksPerSecond, "ticksPerSecond");
  if (framesPerSecond.numerator <= 0n) throw new RangeError("framesPerSecond must be positive");
  if (ticksPerSecond.numerator <= 0n) throw new RangeError("ticksPerSecond must be positive");
  const frameAtBoundary = (tick) => {
    const frame = new Rational(BigInt(tick))
      .multiply(framesPerSecond)
      .divide(ticksPerSecond)
      .ceil();
    if (frame > BigInt(Number.MAX_SAFE_INTEGER)) {
      throw new RangeError("temporal work-unit frame index exceeds the safe integer range");
    }
    return Number(frame);
  };
  const finalFrameEnd = frameAtBoundary(totalTicks) + 1;
  const units = [];
  if (totalTicks === 0) {
    return Object.freeze([Object.freeze({
      index: 0,
      startBoundaryTick: 0,
      endBoundaryTick: 0,
      frameStart: 0,
      frameEndExclusive: 1,
      frameCount: 1,
      final: true,
      pixiResumable: false,
    })]);
  }
  for (let startTick = 0, index = 0; startTick < totalTicks; startTick += ticksPerUnit, index++) {
    const endTick = Math.min(totalTicks, startTick + ticksPerUnit);
    const final = endTick === totalTicks;
    const frameStart = startTick === 0 ? 0 : frameAtBoundary(startTick);
    const frameEndExclusive = final ? finalFrameEnd : frameAtBoundary(endTick);
    units.push(Object.freeze({
      index,
      startBoundaryTick: startTick,
      endBoundaryTick: endTick,
      frameStart,
      frameEndExclusive,
      frameCount: frameEndExclusive - frameStart,
      final,
      // A raw replay state is not a complete action/ticker/PRNG checkpoint.
      // Only a future stateless absolute-time ReplayIR backend may run these
      // units independently or out of order.
      pixiResumable: false,
    }));
  }
  return Object.freeze(units);
}

module.exports = {
  compileStateBatch,
  mapConcurrent,
  planFrameBatches,
  planReplayChunkEnds,
  planStateBatches,
  planTemporalWorkUnits,
  preloadReplayChunks,
  prepareStateBatches,
  reconstructStateBatch,
};
