"use strict";

const {
  evaluateRendererExpression,
  resolvePath,
} = require("./renderer-expressions");
const { replayObjectId } = require("./replay-ir");

function rendererEqual(left, right, seen = new Map()) {
  if (left === right) return true;
  if (Number.isNaN(left) && Number.isNaN(right)) return true;
  if (!left || !right || typeof left !== "object" || typeof right !== "object") {
    return false;
  }
  if (Array.isArray(left) !== Array.isArray(right)) return false;
  let rights = seen.get(left);
  if (rights && rights.has(right)) return true;
  if (!rights) {
    rights = new Set();
    seen.set(left, rights);
  }
  rights.add(right);
  if (Array.isArray(left)) {
    if (left.length !== right.length) return false;
    for (let index = 0; index < left.length; index++) {
      if (!rendererEqual(left[index], right[index], seen)) return false;
    }
    return true;
  }
  const leftKeys = Object.keys(left);
  const rightKeys = Object.keys(right);
  if (leftKeys.length !== rightKeys.length) return false;
  for (const key of leftKeys) {
    if (!Object.prototype.hasOwnProperty.call(right, key)
      || !rendererEqual(left[key], right[key], seen)) {
      return false;
    }
  }
  return true;
}

function parseRendererValue(value, stateParams, random, rejectFunctionRandom = false) {
  if (typeof value === "function") {
    if (rejectFunctionRandom
      && /\bMath\s*\.\s*random\s*\(/.test(Function.prototype.toString.call(value))) {
      throw new Error("random renderer calculation functions cannot be precomputed exactly");
    }
    return value(stateParams);
  }
  return evaluateRendererExpression(value, stateParams, random);
}

function propsChanged(metadata, stateParams) {
  const props = metadata.props === undefined ? "*" : metadata.props;
  const {
    prevState,
    state,
    prevCalcs,
    calcs,
  } = stateParams;
  if (!prevState) return true;
  if (props === "*") return true;
  if (!Array.isArray(props)) {
    throw new TypeError("renderer calculation props must be '*' or an array");
  }
  return props.some(
    (property) => !rendererEqual(prevState[property], state[property]),
  ) || props.some(
    (property) => !rendererEqual(prevCalcs[property], calcs[property]),
  );
}

function shouldRun(metadata, stateParams, changed, random, rejectFunctionRandom = false) {
  const when = metadata.when === undefined ? metadata.shouldRun : metadata.when;
  if (when && !parseRendererValue(
    when,
    stateParams,
    random,
    rejectFunctionRandom,
  )) return false;
  if (!stateParams.prevState || !stateParams.state) return true;
  return changed;
}

function mergeObjectMetadata(metadata, type) {
  const objectMetadata = metadata.objects[type];
  if (!objectMetadata || typeof objectMetadata !== "object") {
    throw new Error(`renderer metadata does not support object type ${type}`);
  }
  if (objectMetadata._initialized) return objectMetadata;
  const common = metadata.objects._all || {};
  return {
    ...objectMetadata,
    actions: [
      ...(common.actions || []),
      ...(objectMetadata.actions || []),
    ],
    calculations: [
      ...(common.calculations || []),
      ...(objectMetadata.calculations || []),
    ],
    data: {
      ...(common.data || {}),
      ...(objectMetadata.data || {}),
    },
    processors: [
      ...(common.processors || []),
      ...(objectMetadata.processors || []),
    ],
  };
}

function createRendererRecord(type) {
  return {
    type,
    state: undefined,
    calcs: {},
    rootContainer: {},
    scope: { actions: {}, processors: {} },
  };
}

function rendererRegistryEntries(records) {
  // The official renderer stores GameObjects in a plain object and applies
  // Object.values(). JavaScript enumerates array-index property names
  // numerically before other names, regardless of insertion order.
  const registry = Object.create(null);
  for (const [id, record] of records) registry[id] = record;
  return Object.keys(registry).map((id) => [id, registry[id]]);
}

class RendererCalculationEvaluator {
  constructor(options) {
    if (!options || typeof options !== "object") {
      throw new TypeError("renderer calculation options are required");
    }
    if (!options.metadata || typeof options.metadata !== "object"
      || !options.metadata.objects || typeof options.metadata.objects !== "object") {
      throw new TypeError("renderer metadata with an objects map is required");
    }
    this.metadata = options.metadata;
    this.world = options.world || { options: {} };
    this.random = options.random || Math.random;
    if (typeof this.random !== "function") throw new TypeError("random must be a function");
    this.rejectFunctionRandom = options.rejectFunctionRandom === true;
    this.getRandomState = options.getRandomState;
    this.setRandomState = options.setRandomState;
    if (this.rejectFunctionRandom
      && (typeof this.getRandomState !== "function"
        || typeof this.setRandomState !== "function")) {
      throw new TypeError(
        "getRandomState and setRandomState are required when rejecting calculation randomness",
      );
    }
    if (options.records !== undefined && !(options.records instanceof Map)) {
      throw new TypeError("records must be a Map");
    }
    this.records = options.records || new Map();
    this.metadataByType = new Map();
  }

  objectMetadata(type) {
    if (!this.metadataByType.has(type)) {
      this.metadataByType.set(type, mergeObjectMetadata(this.metadata, type));
    }
    return this.metadataByType.get(type);
  }

  evaluateTick(state, tickDuration) {
    return this.withRandomGuard(() => this.evaluateTickUnchecked(state, tickDuration));
  }

  withRandomGuard(callback) {
    if (typeof callback !== "function") throw new TypeError("callback must be a function");
    if (!this.rejectFunctionRandom) return callback();
    const originalGlobalRandom = Math.random;
    const originalInjectedRandom = this.random;
    const randomStateBefore = this.getRandomState();
    let randomAttempted = false;
    const rejectRandom = () => {
      randomAttempted = true;
      throw new Error("random renderer calculations cannot be precomputed exactly");
    };
    Math.random = rejectRandom;
    this.random = rejectRandom;
    let result;
    let failure;
    let randomStateAfter;
    try {
      result = callback();
    } catch (error) {
      failure = error;
    } finally {
      this.random = originalInjectedRandom;
      Math.random = originalGlobalRandom;
      randomStateAfter = this.getRandomState();
      if (randomStateAfter !== randomStateBefore) {
        this.setRandomState(randomStateBefore);
      }
    }
    if (failure) throw failure;
    if (randomAttempted) {
      throw new Error("random renderer calculations cannot be precomputed exactly");
    }
    if (randomStateAfter !== randomStateBefore) {
      throw new Error("captured renderer randomness cannot be precomputed exactly");
    }
    return result;
  }

  prepareTick(state, tickDuration) {
    if (!state || typeof state !== "object" || !Array.isArray(state.objects)) {
      throw new TypeError("renderer calculation state must contain an objects array");
    }
    tickDuration = Number(tickDuration);
    if (!Number.isFinite(tickDuration) || tickDuration < 0) {
      throw new RangeError("tickDuration must be a nonnegative finite number");
    }

    // A few official calculations memoize shared tick data on stateExtra.
    // Preserve that within this evaluation while keeping ReplayIR source states
    // immutable from the compiler's perspective.
    const stateExtra = {
      ...state,
      // World.applyState installs renderer game data before preprocessors and
      // before any GameObject calculation runs.
      gameData: this.world.options && this.world.options.gameData || {},
    };
    const objectFilter = this.world.options && this.world.options.objectFilter;
    const objects = objectFilter ? objectFilter(stateExtra.objects) : stateExtra.objects;
    if (!Array.isArray(objects)) {
      throw new TypeError("renderer objectFilter must return an array");
    }
    if (objects.length !== stateExtra.objects.length
      || objects.some((object, index) => object !== stateExtra.objects[index])) {
      throw new Error(
        "renderer objectFilter changes the scene and requires the full processor compiler",
      );
    }
    const statesById = new Map();

    for (const objectState of objects) {
      const id = replayObjectId(objectState);
      if (statesById.has(id)) {
        throw new Error(`duplicate object identity ${id} in renderer calculations`);
      }
      statesById.set(id, objectState);
      const type = objectState.type;
      // Validate before mutating the persistent registry so a failed tick does
      // not poison subsequent evaluation attempts.
      this.objectMetadata(type);
      let record = this.records.get(id);
      if (record && record.type !== type) {
        throw new Error(`renderer object ${id} changed type from ${record.type} to ${type}`);
      }
      if (!record) {
        record = createRendererRecord(type);
        this.records.set(id, record);
      }
    }
    return {
      objects,
      stateExtra,
      statesById,
      tickDuration,
    };
  }

  evaluateObject(record, objectState, prepared) {
    const objectMetadata = this.objectMetadata(record.type);
    const prevState = record.state;
    const prevCalcs = record.calcs;
    const calcs = { ...prevCalcs };
    const stateParams = {
      calcs,
      firstRun: !prevState,
      objectMetadata,
      prevCalcs,
      prevState,
      world: this.world,
      rootContainer: record.rootContainer,
      scope: record.scope,
      state: objectState,
      stateExtra: prepared.stateExtra,
      tickDuration: prepared.tickDuration,
    };

    if (!prevState && objectMetadata.data) {
      for (const [key, value] of Object.entries(objectMetadata.data)) {
        record.rootContainer[key] = parseRendererValue(
          value,
          stateParams,
          this.random,
          this.rejectFunctionRandom,
        );
      }
    }

    for (const calculation of objectMetadata.calculations || []) {
      const changed = propsChanged(calculation, stateParams);
      if (!shouldRun(
        calculation,
        stateParams,
        changed,
        this.random,
        this.rejectFunctionRandom,
      )) continue;
      const path = calculation.path === undefined ? null : calculation.path;
      const calculationParams = {
        ...stateParams,
        state: path === null ? objectState : resolvePath(objectState, path),
        prevState: path === null ? prevState : resolvePath(prevState, path),
        payload: calculation.payload,
      };
      const id = calculation.id === undefined ? "customField" : calculation.id;
      calcs[id] = parseRendererValue(
        calculation.func,
        calculationParams,
        this.random,
        this.rejectFunctionRandom,
      );
    }
    return { calcs, objectMetadata, stateParams };
  }

  commitObject(record, objectState, evaluated) {
    record.state = objectState;
    record.calcs = evaluated.calcs;
  }

  evaluateTickUnchecked(state, tickDuration) {
    const prepared = this.prepareTick(state, tickDuration);
    const results = new Map();
    // Match Object.values(world.gameObjects), including integer-like IDs.
    for (const [id, record] of rendererRegistryEntries(this.records)) {
      const objectState = prepared.statesById.get(id);
      if (!objectState) {
        this.records.delete(id);
        continue;
      }
      const evaluated = this.evaluateObject(record, objectState, prepared);
      this.commitObject(record, objectState, evaluated);
      results.set(id, evaluated.calcs);
    }
    return results;
  }
}

function evaluateRendererCalculations(options) {
  if (!options || typeof options !== "object") throw new TypeError("options are required");
  const states = options.states instanceof Map
    ? [...options.states.entries()]
      .sort(([left], [right]) => left - right)
      .map(([, state]) => state)
    : options.states;
  if (!Array.isArray(states)) throw new TypeError("states must be an array or Map");
  const evaluator = new RendererCalculationEvaluator(options);
  return states.map((state) => evaluator.evaluateTick(state, options.tickDuration));
}

module.exports = {
  RendererCalculationEvaluator,
  createRendererRecord,
  evaluateRendererCalculations,
  mergeObjectMetadata,
  rendererRegistryEntries,
  parseRendererValue,
  propsChanged,
  rendererEqual,
  shouldRun,
};
