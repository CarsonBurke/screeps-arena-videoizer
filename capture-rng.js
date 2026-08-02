"use strict";

const {
  createRendererRandom,
  hashRendererSeed,
} = require("./renderer-random");

function preserveRendererRandomState(callback) {
  if (typeof callback !== "function") throw new TypeError("callback must be a function");
  const stateKey = "__screepsArenaVideoizerRandomState";
  const hasState = typeof globalThis !== "undefined"
    && Number.isInteger(globalThis[stateKey]);
  const state = hasState ? globalThis[stateKey] : null;
  const random = Math.random;
  // Always snapshot closed-over stream state when available. installSeeded
  // keeps both global and function state; restoring only the global key left
  // Math.random on a diverged mulberry32 stream.
  const functionState = typeof random.getState === "function"
    ? random.getState()
    : null;
  try {
    return callback();
  } finally {
    Math.random = random;
    if (functionState !== null && typeof random.setState === "function") {
      random.setState(functionState);
    }
    if (hasState) {
      globalThis[stateKey] = functionState !== null
        ? (typeof random.getState === "function" ? random.getState() : functionState)
        : state;
    }
  }
}

function rendererRandomState() {
  // Prefer the active Math.random stream when it exposes state. The boot seeder
  // mutates the global key without getState; installSeededRendererRandom keeps
  // both views synchronized.
  if (typeof Math.random.getState === "function") {
    return Math.random.getState() >>> 0;
  }
  const stateKey = "__screepsArenaVideoizerRandomState";
  if (typeof globalThis !== "undefined" && Number.isInteger(globalThis[stateKey])) {
    return globalThis[stateKey] >>> 0;
  }
  return null;
}

/**
 * Install a mulberry32 stream that stays consistent with both closed-over
 * getState/setState helpers and the global `__screepsArenaVideoizerRandomState`
 * key used by the early boot seeder in the patched client.
 */
function installSeededRendererRandom(seed) {
  const hashed = hashRendererSeed(seed);
  const random = createRendererRandom(hashed);
  if (typeof globalThis !== "undefined") {
    globalThis.__screepsArenaVideoizerRandomState = hashed;
    globalThis.__screepsArenaVideoizerRandomSeed = String(seed);
  }
  const next = () => {
    const value = random();
    if (typeof globalThis !== "undefined") {
      globalThis.__screepsArenaVideoizerRandomState = random.getState();
    }
    return value;
  };
  next.getState = () => random.getState();
  next.setState = (value) => {
    random.setState(value);
    if (typeof globalThis !== "undefined") {
      globalThis.__screepsArenaVideoizerRandomState = random.getState();
    }
  };
  Math.random = next;
  return hashed;
}

function setRendererRandomState(state) {
  const stateKey = "__screepsArenaVideoizerRandomState";
  // When Math.random owns a closed-over stream, update that first. Updating only
  // the global key leaves createRendererRandom/installSeeded streams stale.
  if (typeof Math.random.setState === "function") {
    Math.random.setState(state);
    if (typeof globalThis !== "undefined" && Number.isInteger(globalThis[stateKey])) {
      globalThis[stateKey] = Math.random.getState
        ? Math.random.getState() >>> 0
        : Number(state) >>> 0;
    }
    return;
  }
  if (typeof globalThis !== "undefined" && Number.isInteger(globalThis[stateKey])) {
    globalThis[stateKey] = Number(state) | 0;
    return;
  }
  throw new Error("board capture: renderer random state is not restorable");
}

module.exports = {
  installSeededRendererRandom,
  preserveRendererRandomState,
  rendererRandomState,
  setRendererRandomState,
};
