"use strict";

function hashRendererSeed(value) {
  let hash = 2166136261;
  for (const character of String(value)) {
    hash ^= character.codePointAt(0);
    hash = Math.imul(hash, 16777619);
  }
  return hash >>> 0;
}

function createRendererRandom(initialState) {
  let state = Number(initialState) >>> 0;
  const random = () => {
    state = (state + 0x6D2B79F5) | 0;
    let value = Math.imul(state ^ state >>> 15, 1 | state);
    value ^= value + Math.imul(value ^ value >>> 7, 61 | value);
    return ((value ^ value >>> 14) >>> 0) / 4294967296;
  };
  random.getState = () => state >>> 0;
  random.setState = (value) => {
    state = Number(value) >>> 0;
  };
  return random;
}

module.exports = {
  createRendererRandom,
  hashRendererSeed,
};
