"use strict";

const crypto = require("node:crypto");

const REPLAY_IR_SCHEMA = "screeps-arena-replay-ir";
const REPLAY_IR_VERSION = 8;
const RENDERER_CONTRACT_SCHEMA = "screeps-arena-renderer-contract";
const RENDERER_CONTRACT_VERSION = 5;
const RENDERER_EVENT_OPS = Object.freeze([
  "action:finish",
  "action:run",
  "object:alpha",
  "object:create",
  "object:remove",
  "preprocessor:run",
  "processor:destruct",
  "processor:run",
]);
const REQUIRED_INVENTORY_KEYS = Object.freeze([
  "actionTypes",
  "calculationIds",
  "drawingMethods",
  "expressionOperators",
  "functionSemantics",
  "layerIds",
  "objectTypes",
  "preprocessors",
  "processorTypes",
  "rendererImplementationFingerprints",
]);
const verifiedContracts = new WeakSet();
const verifiedReplays = new WeakSet();
const RUNTIME_WORLD_OPTION_KEYS = new Set([
  "actionManager",
  "app",
  "logger",
  "metadata",
  "objectFilter",
  "resourceMap",
]);

function rendererEventShapeValid(entityId, opcode, semanticId) {
  if (opcode === "preprocessor:run") {
    return entityId === null && semanticId !== null;
  }
  if (opcode.startsWith("object:")) {
    return entityId !== null && semanticId === null;
  }
  return entityId !== null && semanticId !== null;
}

function asNonnegativeInteger(value, name) {
  const number = Number(value);
  if (!Number.isSafeInteger(number) || number < 0) {
    throw new RangeError(`${name} must be a nonnegative safe integer`);
  }
  return number;
}

function normalizeRandomState(value) {
  if (value === undefined || value === null) return null;
  const number = Number(value);
  if (!Number.isSafeInteger(number) || number < 0 || number > 0xffffffff) {
    throw new RangeError("randomStateAtFirstTick must be an unsigned 32-bit integer");
  }
  return number;
}

const BOARD_FRAME_KEYS = Object.freeze([
  "boardHeight",
  "boardWidth",
  "bottom",
  "height",
  "left",
  "mode",
  "outputHeight",
  "outputWidth",
  "padding",
  "panX",
  "panY",
  "pivotX",
  "pivotY",
  "right",
  "top",
  "width",
  "worldMinX",
  "worldMinY",
  "x",
  "y",
  "zoom",
]);

function validateRenderConfig(renderConfig) {
  if (renderConfig === null) return true;
  if (!renderConfig || typeof renderConfig !== "object" || Array.isArray(renderConfig)
    || Object.keys(renderConfig).sort().join(",")
      !== "backgroundColor,boardFrame,height,width") {
    throw new Error("invalid ReplayIR renderConfig");
  }
  if (!Number.isSafeInteger(renderConfig.width) || renderConfig.width <= 0
    || !Number.isSafeInteger(renderConfig.height) || renderConfig.height <= 0
    || !Number.isSafeInteger(renderConfig.backgroundColor)
    || renderConfig.backgroundColor < 0 || renderConfig.backgroundColor > 0xffffff) {
    throw new Error("invalid ReplayIR renderConfig geometry");
  }
  const frame = renderConfig.boardFrame;
  if (!frame || typeof frame !== "object" || Array.isArray(frame)
    || Object.keys(frame).sort().join(",") !== BOARD_FRAME_KEYS.join(",")) {
    throw new Error("invalid ReplayIR boardFrame");
  }
  if (!["auto", "manual"].includes(frame.mode)
    || frame.outputWidth !== renderConfig.width
    || frame.outputHeight !== renderConfig.height) {
    throw new Error("invalid ReplayIR boardFrame mode/extent");
  }
  for (const key of BOARD_FRAME_KEYS) {
    if (key === "mode") continue;
    if (typeof frame[key] !== "number" || !Number.isFinite(frame[key])) {
      throw new Error(`invalid ReplayIR boardFrame ${key}`);
    }
  }
  if (frame.outputWidth <= 0 || frame.outputHeight <= 0
    || frame.boardWidth <= 0 || frame.boardHeight <= 0
    || frame.zoom <= 0 || frame.width <= 0 || frame.height <= 0
    || frame.padding < 0) {
    throw new Error("invalid ReplayIR boardFrame geometry");
  }
  return true;
}

function normalizeRenderConfig(value) {
  if (value === undefined || value === null) return null;
  const normalized = canonicalizeJSON(value, "$.renderConfig");
  validateRenderConfig(normalized);
  return normalized;
}

/**
 * Canonicalize a value for fingerprinting / storage.
 *
 * Modes differ only in how non-JSON leaves are handled:
 * - full: bigint/function/undefined become tagged objects
 * - json: reject non-JSON leaves
 * - event: undefined is tagged; function/bigint rejected
 */
function canonicalizeValue(value, mode = "full", path = "$", seen = new Set()) {
  if (value === null || typeof value === "string" || typeof value === "boolean") {
    return value;
  }
  if (typeof value === "number") {
    if (!Number.isFinite(value)) throw new TypeError(`${path} contains a non-finite number`);
    return Object.is(value, -0) ? 0 : value;
  }
  if (value === undefined) {
    if (mode === "json") throw new TypeError(`${path} contains non-JSON undefined`);
    return { $undefined: true };
  }
  if (typeof value === "bigint") {
    if (mode === "full") return { $bigint: value.toString() };
    throw new TypeError(
      mode === "event"
        ? `${path} contains non-event bigint`
        : `${path} contains non-JSON bigint`,
    );
  }
  if (typeof value === "function") {
    if (mode === "full") {
      return { $function: Function.prototype.toString.call(value) };
    }
    throw new TypeError(
      mode === "event"
        ? `${path} contains non-event function`
        : `${path} contains non-JSON function`,
    );
  }
  if (typeof value !== "object") {
    throw new TypeError(`${path} contains unsupported ${typeof value}`);
  }
  if (seen.has(value)) throw new TypeError(`${path} contains a cyclic value`);
  seen.add(value);
  let result;
  if (Array.isArray(value)) {
    result = value.map(
      (item, index) => canonicalizeValue(item, mode, `${path}[${index}]`, seen),
    );
  } else {
    result = {};
    for (const key of Object.keys(value).sort()) {
      result[key] = canonicalizeValue(value[key], mode, `${path}.${key}`, seen);
    }
  }
  seen.delete(value);
  return result;
}

function canonicalize(value, path = "$", seen = new Set()) {
  return canonicalizeValue(value, "full", path, seen);
}

function canonicalizeJSON(value, path = "$", seen = new Set()) {
  return canonicalizeValue(value, "json", path, seen);
}

function canonicalizeCalculationValue(value, path = "$", seen = new Set()) {
  const nonFinite = [];
  const encode = (current, sourcePath, pointer) => {
    if (typeof current === "number" && !Number.isFinite(current)) {
      nonFinite.push([pointer, Number.isNaN(current) ? 0 : current < 0 ? -1 : 1]);
      return null;
    }
    if (current === null || typeof current === "string" || typeof current === "boolean"
      || typeof current === "number") {
      return Object.is(current, -0) ? 0 : current;
    }
    if (current === undefined || typeof current === "function" || typeof current === "bigint") {
      throw new TypeError(`${sourcePath} contains nested non-JSON ${typeof current}`);
    }
    if (!current || typeof current !== "object" || seen.has(current)) {
      throw new TypeError(`${sourcePath} contains a cyclic or unsupported value`);
    }
    seen.add(current);
    let result;
    if (Array.isArray(current)) {
      result = current.map((item, index) => encode(
        item,
        `${sourcePath}[${index}]`,
        `${pointer}/${index}`,
      ));
    } else {
      result = {};
      for (const key of Object.keys(current).sort()) {
        const token = key.replaceAll("~", "~0").replaceAll("/", "~1");
        result[key] = encode(current[key], `${sourcePath}.${key}`, `${pointer}/${token}`);
      }
    }
    seen.delete(current);
    return result;
  };
  return { value: encode(value, path, ""), nonFinite };
}

function canonicalizeRendererEventValue(value, path = "$", seen = new Set()) {
  return canonicalizeValue(value, "event", path, seen);
}

function stableStringify(value) {
  return JSON.stringify(canonicalize(value));
}

function fingerprint(value) {
  return crypto.createHash("sha256").update(stableStringify(value)).digest("hex");
}

function fingerprintDeterministic(value) {
  return crypto.createHash("sha256").update(JSON.stringify(value)).digest("hex");
}

function deepFreeze(value, seen = new Set()) {
  if (!value || typeof value !== "object" || Object.isFrozen(value) || seen.has(value)) {
    return value;
  }
  seen.add(value);
  if (Array.isArray(value)) {
    for (let index = 0; index < value.length; index++) {
      const child = value[index];
      if (child && typeof child === "object") deepFreeze(child, seen);
    }
  } else {
    for (const key of Object.keys(value)) {
      const child = value[key];
      if (child && typeof child === "object") deepFreeze(child, seen);
    }
  }
  return Object.freeze(value);
}

function replayObjectId(object) {
  if (!object || typeof object !== "object") {
    throw new TypeError("replay object must be an object");
  }
  if (object._id !== undefined && object._id !== null) return String(object._id);
  for (const key of ["room", "type", "x", "y"]) {
    if (object[key] === undefined) {
      throw new Error(`object without _id is missing identity field ${key}`);
    }
  }
  return `${object.room}:${object.type}:${object.x}:${object.y}`;
}

function readStates(states, totalTicks) {
  if (states instanceof Map) {
    return Array.from({ length: totalTicks + 1 }, (_, tick) => states.get(tick));
  }
  if (Array.isArray(states)) return states;
  throw new TypeError("states must be a Map keyed by tick or an array");
}

function createTrack() {
  return {
    bounds: [],
    values: [],
    absent: [],
    undefined: [],
    nonFinite: [],
    lastKind: null,
    lastPrimitive: null,
    lastNonFinite: null,
  };
}

function replayValueEqualsCanonical(value, canonical, path, seen = new Set()) {
  if (value === null || typeof value === "string" || typeof value === "boolean") {
    return Object.is(value, canonical);
  }
  if (typeof value === "number") {
    if (!Number.isFinite(value)) throw new TypeError(`${path} contains a non-finite number`);
    return Object.is(Object.is(value, -0) ? 0 : value, canonical);
  }
  if (value === undefined || typeof value === "function" || typeof value === "bigint") {
    throw new TypeError(`${path} contains nested non-JSON ${typeof value}`);
  }
  if (!value || typeof value !== "object" || seen.has(value)) {
    throw new TypeError(`${path} contains a cyclic or unsupported value`);
  }
  if (!canonical || typeof canonical !== "object" || Array.isArray(value) !== Array.isArray(canonical)) {
    return false;
  }
  seen.add(value);
  let equal = true;
  if (Array.isArray(value)) {
    equal = value.length === canonical.length;
    for (let index = 0; equal && index < value.length; index++) {
      equal = replayValueEqualsCanonical(value[index], canonical[index], path, seen);
    }
  } else {
    const keys = Object.keys(value);
    equal = keys.length === Object.keys(canonical).length;
    for (const key of keys) {
      if (!equal || !Object.prototype.hasOwnProperty.call(canonical, key)) {
        equal = false;
        break;
      }
      equal = replayValueEqualsCanonical(value[key], canonical[key], path, seen);
    }
  }
  seen.delete(value);
  return equal;
}

function storedJSONEquals(left, right) {
  if (Object.is(left, right)) return true;
  if (!left || !right || typeof left !== "object" || typeof right !== "object"
    || Array.isArray(left) !== Array.isArray(right)) return false;
  if (Array.isArray(left)) {
    return left.length === right.length
      && left.every((value, index) => storedJSONEquals(value, right[index]));
  }
  const keys = Object.keys(left);
  return keys.length === Object.keys(right).length
    && keys.every((key) => Object.prototype.hasOwnProperty.call(right, key)
      && storedJSONEquals(left[key], right[key]));
}

function nonFiniteEntriesEqual(left, right) {
  return Array.isArray(left) && Array.isArray(right) && left.length === right.length
    && left.every((entry, index) => entry[0] === right[index][0] && entry[1] === right[index][1]);
}

function appendSegment(track, tick, value, present = true, path = "$", allowNonFinite = false) {
  let kind = "absent";
  let primitive = null;
  let encodedCalculation = null;
  if (present) {
    if (value === undefined) kind = "undefined";
    else if (value === null) kind = "null";
    else if (typeof value === "number") {
      if (!Number.isFinite(value)) {
        if (!allowNonFinite) throw new TypeError(`${path} contains a non-finite number`);
        kind = "nonFinite";
        primitive = Number.isNaN(value) ? 0 : value < 0 ? -1 : 1;
      } else {
        kind = "number";
        primitive = Object.is(value, -0) ? 0 : value;
      }
    } else if (typeof value === "boolean") {
      kind = "boolean";
      primitive = value;
    } else if (typeof value === "string") {
      kind = "string";
      primitive = value;
    } else {
      kind = "complex";
      if (allowNonFinite) encodedCalculation = canonicalizeCalculationValue(value, path);
    }
  }
  const segmentIndex = track.values.length - 1;
  const contiguousAndSameKind = segmentIndex >= 0
    && track.bounds[segmentIndex * 2 + 1] === tick
    && track.lastKind === kind;
  const unchanged = contiguousAndSameKind && (kind === "complex"
    ? allowNonFinite
      ? storedJSONEquals(encodedCalculation.value, track.values[segmentIndex])
        && nonFiniteEntriesEqual(encodedCalculation.nonFinite, track.lastNonFinite)
      : replayValueEqualsCanonical(value, track.values[segmentIndex], path)
    : Object.is(track.lastPrimitive, primitive));
  if (unchanged) {
    track.bounds[segmentIndex * 2 + 1] = tick + 1;
    return;
  }
  if (kind === "nonFinite") encodedCalculation = canonicalizeCalculationValue(value, path);
  const storedValue = present && value !== undefined
    ? deepFreeze(allowNonFinite
      ? (encodedCalculation || canonicalizeCalculationValue(value, path)).value
      : canonicalizeJSON(value, path))
    : null;
  const nextIndex = track.values.length;
  track.bounds.push(tick, tick + 1);
  track.values.push(storedValue);
  if (!present) track.absent.push(nextIndex);
  if (present && value === undefined) track.undefined.push(nextIndex);
  const nonFinite = encodedCalculation && encodedCalculation.nonFinite || [];
  for (const [pointer, code] of nonFinite) track.nonFinite.push([nextIndex, pointer, code]);
  track.lastKind = kind;
  track.lastPrimitive = primitive;
  track.lastNonFinite = nonFinite;
}

function finishTrack(track) {
  return Object.freeze([
    Object.freeze(track.bounds),
    Object.freeze(track.values),
    Object.freeze(track.absent),
    Object.freeze(track.undefined),
    Object.freeze(track.nonFinite.map((entry) => Object.freeze(entry))),
  ]);
}

function appendLifetime(lifetimes, tick) {
  const lifetime = lifetimes.at(-1);
  if (lifetime && lifetime.endTick === tick) {
    lifetime.endTick = tick + 1;
  } else {
    lifetimes.push({ startTick: tick, endTick: tick + 1 });
  }
}

function extractActionEvents(actionLog, tick, entityId, events) {
  if (actionLog === undefined || actionLog === null) return;
  if (typeof actionLog !== "object" || Array.isArray(actionLog)) {
    throw new TypeError(`actionLog for ${entityId} at tick ${tick} must be an object`);
  }
  for (const name of Object.keys(actionLog).sort()) {
    const payload = actionLog[name];
    if (payload === undefined || payload === null || payload === false) continue;
    // The lossless payload already lives in the actionLog property track. Keep
    // only an event index here so render backends can find active effects
    // without scanning every entity while avoiding a second payload copy.
    events.push(Object.freeze([tick, entityId, name]));
  }
}

function appendObjectTracks(tracks, activeKeys, value, tick, path, allowNonFinite = false) {
  const keys = Object.keys(value);
  const sameKeys = keys.length === activeKeys.length
    && keys.every((key, index) => key === activeKeys[index]);
  if (sameKeys) {
    for (const key of keys) {
      appendSegment(
        tracks.get(key), tick, value[key], true, `${path}.${key}`, allowNonFinite,
      );
    }
    return keys;
  }
  const presentKeys = new Set(keys);
  for (const key of activeKeys) {
    if (!presentKeys.has(key)) {
      appendSegment(
        tracks.get(key), tick, undefined, false, `${path}.${key}`, allowNonFinite,
      );
    }
  }
  for (const key of keys) {
    let track = tracks.get(key);
    if (!track) {
      track = createTrack();
      tracks.set(key, track);
    }
    appendSegment(track, tick, value[key], true, `${path}.${key}`, allowNonFinite);
  }
  return keys;
}

function readCalculationStates(calculationStates, totalTicks) {
  if (calculationStates === undefined || calculationStates === null) return null;
  const states = readStates(calculationStates, totalTicks);
  if (states.length !== totalTicks + 1) {
    throw new Error(`expected ${totalTicks + 1} calculation states, got ${states.length}`);
  }
  return states;
}

function compileEntityTracks(
  states,
  totalTicks,
  includeTickFingerprints,
  calculationStates,
  calculationEvaluator,
  rendererEventEvaluator,
  rendererTickEvaluator,
) {
  const entities = new Map();
  const objectOrderSegments = createTrack();
  const actionEvents = [];
  const tickFingerprints = [];
  const rendererEventEntityIds = [];
  const rendererEventEntityIndex = new Map();
  const rendererEventSemanticIds = [];
  const rendererEventSemanticIndex = new Map();
  const rendererEventPayloads = [];
  const rendererEventColumns = [[], [], [], []];
  const rendererEventOffsets = new Array(totalTicks + 2).fill(0);
  const compiledCalculations = readCalculationStates(calculationStates, totalTicks);
  const calculationsEnabled = compiledCalculations !== null
    || calculationEvaluator !== undefined
    || rendererTickEvaluator !== undefined;
  if (compiledCalculations && calculationEvaluator) {
    throw new Error("calculationStates and calculationEvaluator are mutually exclusive");
  }
  if (rendererTickEvaluator !== undefined
    && (compiledCalculations || calculationEvaluator || rendererEventEvaluator)) {
    throw new Error(
      "rendererTickEvaluator is mutually exclusive with separate calculation/event evaluators",
    );
  }
  if (calculationEvaluator !== undefined && typeof calculationEvaluator !== "function") {
    throw new TypeError("calculationEvaluator must be a function");
  }
  if (rendererEventEvaluator !== undefined && typeof rendererEventEvaluator !== "function") {
    throw new TypeError("rendererEventEvaluator must be a function");
  }
  if (rendererTickEvaluator !== undefined && typeof rendererTickEvaluator !== "function") {
    throw new TypeError("rendererTickEvaluator must be a function");
  }

  for (let tick = 0; tick <= totalTicks; tick++) {
    rendererEventOffsets[tick] = rendererEventColumns[0].length;
    const state = states[tick];
    if (!state || typeof state !== "object" || !Array.isArray(state.objects)) {
      throw new Error(`missing or invalid replay state ${tick}`);
    }
    const seenIds = new Set();
    const order = [];
    let calculations = compiledCalculations
      ? compiledCalculations[tick]
      : calculationEvaluator
        ? calculationEvaluator(state, tick)
        : null;
    let tickEvents = null;
    if (rendererTickEvaluator) {
      const evaluated = rendererTickEvaluator(state, tick);
      if (!evaluated || typeof evaluated !== "object"
        || !(evaluated.calculations instanceof Map)
        || !Array.isArray(evaluated.events)) {
        throw new TypeError(
          `renderer tick evaluation ${tick} must contain calculations Map and events array`,
        );
      }
      calculations = evaluated.calculations;
      tickEvents = evaluated.events;
    }
    if (calculationsEnabled && !(calculations instanceof Map)) {
      throw new TypeError(`calculation state ${tick} must be a Map`);
    }
    for (const object of state.objects) {
      const id = replayObjectId(object);
      if (seenIds.has(id)) throw new Error(`duplicate object identity ${id} at tick ${tick}`);
      seenIds.add(id);
      order.push(id);
      let entity = entities.get(id);
      if (!entity) {
        entity = {
          id,
          lifetimes: [],
          properties: new Map(),
          activeKeys: [],
          calculations: new Map(),
          activeCalculationKeys: [],
        };
        entities.set(id, entity);
      }
      appendLifetime(entity.lifetimes, tick);

      entity.activeKeys = appendObjectTracks(
        entity.properties,
        entity.activeKeys,
        object,
        tick,
        `$.states[${tick}].objects.${id}`,
      );
      if (calculations) {
        if (!calculations.has(id)) {
          throw new Error(`calculation state ${tick} is missing active object ${id}`);
        }
        const values = calculations.get(id);
        if (!values || typeof values !== "object" || Array.isArray(values)) {
          throw new TypeError(`calculations for ${id} at tick ${tick} must be an object`);
        }
        entity.activeCalculationKeys = appendObjectTracks(
          entity.calculations,
          entity.activeCalculationKeys,
          values,
          tick,
          `$.calculationStates[${tick}].${id}`,
          true,
        );
      }
      extractActionEvents(object.actionLog, tick, id, actionEvents);
    }
    if (calculations) {
      for (const id of calculations.keys()) {
        if (typeof id !== "string" || !seenIds.has(id)) {
          throw new Error(`calculation state ${tick} references inactive object ${id}`);
        }
      }
    }
    if (rendererEventEvaluator || rendererTickEvaluator) {
      if (rendererEventEvaluator) {
        tickEvents = rendererEventEvaluator(state, tick, calculations);
      }
      if (!Array.isArray(tickEvents)) {
        throw new TypeError(`renderer events for tick ${tick} must be an array`);
      }
      for (let index = 0; index < tickEvents.length; index++) {
        const event = tickEvents[index];
        if (!Array.isArray(event) || event.length !== 5
          || event[0] !== tick
          || (event[1] !== null && typeof event[1] !== "string")
          || (event[1] !== null && !entities.has(event[1]))
          || !RENDERER_EVENT_OPS.includes(event[2])
          || (event[3] !== null && typeof event[3] !== "string")
          || !rendererEventShapeValid(event[1], event[2], event[3])) {
          throw new Error(`invalid renderer event ${tick}:${index}`);
        }
        let entityIndex = -1;
        if (event[1] !== null) {
          entityIndex = rendererEventEntityIndex.get(event[1]);
          if (entityIndex === undefined) {
            entityIndex = rendererEventEntityIds.length;
            rendererEventEntityIndex.set(event[1], entityIndex);
            rendererEventEntityIds.push(event[1]);
          }
        }
        let semanticIndex = -1;
        if (event[3] !== null) {
          semanticIndex = rendererEventSemanticIndex.get(event[3]);
          if (semanticIndex === undefined) {
            semanticIndex = rendererEventSemanticIds.length;
            rendererEventSemanticIndex.set(event[3], semanticIndex);
            rendererEventSemanticIds.push(event[3]);
          }
        }
        let payloadIndex = -1;
        if (event[4] !== null) {
          payloadIndex = rendererEventPayloads.length;
          rendererEventPayloads.push(canonicalizeRendererEventValue(
            event[4],
            `$.rendererGraph.payloads[${payloadIndex}]`,
          ));
        }
        rendererEventColumns[0].push(entityIndex);
        rendererEventColumns[1].push(RENDERER_EVENT_OPS.indexOf(event[2]));
        rendererEventColumns[2].push(semanticIndex);
        rendererEventColumns[3].push(payloadIndex);
      }
    }

    appendSegment(objectOrderSegments, tick, order, true, `$.states[${tick}].objectOrder`);
    if (includeTickFingerprints) tickFingerprints.push(fingerprint(state));
  }
  rendererEventOffsets[totalTicks + 1] = rendererEventColumns[0].length;

  const compiledEntities = [...entities.values()]
    .sort((left, right) => left.id.localeCompare(right.id))
    .map((entity) => {
      const properties = {};
      for (const [key, segments] of [...entity.properties].sort(([a], [b]) => a.localeCompare(b))) {
        properties[key] = finishTrack(segments);
      }
      const calculations = {};
      for (const [key, segments] of [...entity.calculations]
        .sort(([a], [b]) => a.localeCompare(b))) {
        calculations[key] = finishTrack(segments);
      }
      return Object.freeze({
        id: entity.id,
        lifetimes: Object.freeze(entity.lifetimes.map(
          (lifetime) => Object.freeze([lifetime.startTick, lifetime.endTick]),
        )),
        properties: Object.freeze(properties),
        calculations: Object.freeze(calculations),
      });
    });

  return Object.freeze({
    entities: Object.freeze(compiledEntities),
    objectOrder: finishTrack(objectOrderSegments),
    actionEvents: Object.freeze(actionEvents),
    rendererEventColumns: Object.freeze(rendererEventColumns.map(
      (column) => Object.freeze(column),
    )),
    rendererEventEntityIds: Object.freeze(rendererEventEntityIds),
    rendererEventOffsets: Object.freeze(rendererEventOffsets),
    rendererEventPayloads: Object.freeze(rendererEventPayloads),
    rendererEventSemanticIds: Object.freeze(rendererEventSemanticIds),
    tickFingerprints: Object.freeze(tickFingerprints),
  });
}

function compileGlobalState(states, totalTicks) {
  const properties = new Map();
  let activeKeys = [];
  for (let tick = 0; tick <= totalTicks; tick++) {
    const { objects: _objects, ...globalState } = states[tick];
    const keys = Object.keys(globalState).sort();
    const allKeys = [...new Set([...activeKeys, ...keys])].sort();
    const present = new Set(keys);
    for (const key of allKeys) {
      let track = properties.get(key);
      if (!track) {
        track = createTrack();
        properties.set(key, track);
      }
      appendSegment(track, tick, globalState[key], present.has(key), `global.${key}`);
    }
    activeKeys = keys;
  }
  const compiled = {};
  for (const [key, track] of [...properties].sort(([left], [right]) => left.localeCompare(right))) {
    compiled[key] = finishTrack(track);
  }
  return Object.freeze(compiled);
}

function compileVisualState(visualStates, totalTicks) {
  const states = visualStates === undefined || visualStates === null
    ? Array.from({ length: totalTicks + 1 }, () => [])
    : readStates(visualStates, totalTicks);
  if (states.length !== totalTicks + 1) {
    throw new Error(`expected ${totalTicks + 1} visual states, got ${states.length}`);
  }
  const segments = createTrack();
  for (let tick = 0; tick <= totalTicks; tick++) {
    if (!Array.isArray(states[tick])) {
      throw new TypeError(`visual state ${tick} must be an array`);
    }
    appendSegment(segments, tick, states[tick], true, `$.visualStates[${tick}]`);
  }
  return finishTrack(segments);
}

function normalizeRate(value, name) {
  if (value === undefined || value === null || value === "") return null;
  if (typeof value === "number" && (!Number.isFinite(value) || value <= 0)) {
    throw new RangeError(`${name} must be positive`);
  }
  const text = String(value);
  if (!/^(?:\d+(?:\.\d+)?|\d+\/\d+)$/.test(text)) {
    throw new RangeError(`${name} must be a positive decimal or rational`);
  }
  const [numerator, denominator = "1"] = text.split("/");
  if (Number(numerator) <= 0 || Number(denominator) <= 0) {
    throw new RangeError(`${name} must be a positive decimal or rational`);
  }
  return text;
}

function compileReplayIR(options) {
  if (!options || typeof options !== "object") throw new TypeError("options must be an object");
  if (options.rendererContract) validateRendererContract(options.rendererContract);
  const inferredTicks = options.states instanceof Map
    ? [...options.states.keys()].reduce((maximum, tick) => Math.max(maximum, tick), -1)
    : Array.isArray(options.states)
      ? options.states.length - 1
      : NaN;
  const totalTicks = asNonnegativeInteger(options.totalTicks ?? inferredTicks, "totalTicks");
  const states = readStates(options.states, totalTicks);
  if (states.length !== totalTicks + 1) {
    throw new Error(`expected ${totalTicks + 1} replay states, got ${states.length}`);
  }

  const compiled = compileEntityTracks(
    states,
    totalTicks,
    options.includeTickFingerprints === true,
    options.calculationStates,
    options.calculationEvaluator,
    options.rendererEventEvaluator,
    options.rendererTickEvaluator,
  );
  const calculationOutputsEnabled = options.calculationStates !== undefined
    && options.calculationStates !== null
    || options.calculationEvaluator !== undefined
    || options.rendererTickEvaluator !== undefined;
  const replay = {
    schema: REPLAY_IR_SCHEMA,
    version: REPLAY_IR_VERSION,
    totalTicks,
    timeline: Object.freeze({
      framesPerSecond: normalizeRate(options.framesPerSecond, "framesPerSecond"),
      ticksPerSecond: normalizeRate(options.ticksPerSecond, "ticksPerSecond"),
      substepsPerSecond: normalizeRate(options.substepsPerSecond, "substepsPerSecond"),
      tickTransitionSeconds: normalizeRate(
        options.tickTransitionSeconds,
        "tickTransitionSeconds",
      ),
    }),
    renderConfig: normalizeRenderConfig(options.renderConfig),
    rendererContractFingerprint: options.rendererContract
      ? options.rendererContract.fingerprint
      : null,
    randomSeed: options.randomSeed === undefined || options.randomSeed === null
      ? null
      : String(options.randomSeed),
    randomStateAtFirstTick: normalizeRandomState(options.randomStateAtFirstTick),
    calculationOutputs: Object.freeze({
      enabled: calculationOutputsEnabled,
    }),
    rendererGraph: Object.freeze({
      columns: compiled.rendererEventColumns,
      enabled: options.rendererEventEvaluator !== undefined
        || options.rendererTickEvaluator !== undefined,
      entityIds: compiled.rendererEventEntityIds,
      offsets: compiled.rendererEventOffsets,
      payloads: compiled.rendererEventPayloads,
      semanticIds: compiled.rendererEventSemanticIds,
    }),
    globalState: compileGlobalState(states, totalTicks),
    visualOverlay: Object.freeze({
      enabled: options.visualOverlayEnabled === true,
      states: compileVisualState(options.visualStates, totalTicks),
    }),
    objectOrder: compiled.objectOrder,
    entities: compiled.entities,
    actionEvents: compiled.actionEvents,
    tickFingerprints: compiled.tickFingerprints,
  };
  // All inserted keys and canonicalized values are deterministic by
  // construction. Hash the serialization directly instead of recursively
  // canonicalizing the (potentially very large) finished IR a second time.
  replay.fingerprint = fingerprintDeterministic(replay);
  const frozenReplay = deepFreeze(replay);
  verifiedReplays.add(frozenReplay);
  return frozenReplay;
}

function sortedArrayIncludes(values, needle) {
  let low = 0;
  let high = values.length - 1;
  while (low <= high) {
    const middle = (low + high) >> 1;
    if (values[middle] < needle) low = middle + 1;
    else if (values[middle] > needle) high = middle - 1;
    else return true;
  }
  return false;
}

function segmentAt(track, tick) {
  const [bounds, values, absent, undefinedValues] = track;
  let low = 0;
  let high = values.length - 1;
  while (low <= high) {
    const middle = (low + high) >> 1;
    const startTick = bounds[middle * 2];
    const endTick = bounds[middle * 2 + 1];
    if (tick < startTick) high = middle - 1;
    else if (tick >= endTick) low = middle + 1;
    else {
      return {
        index: middle,
        present: !sortedArrayIncludes(absent, middle),
        value: sortedArrayIncludes(undefinedValues, middle) ? undefined : values[middle],
      };
    }
  }
  return null;
}

function entityAliveAt(entity, tick) {
  return entity.lifetimes.some(
    (lifetime) => tick >= lifetime[0] && tick < lifetime[1],
  );
}

function reconstructReplayTick(replay, tick) {
  validateReplayIR(replay);
  tick = asNonnegativeInteger(tick, "tick");
  if (tick > replay.totalTicks) throw new RangeError("tick exceeds replay endpoint");
  const orderSegment = segmentAt(replay.objectOrder, tick);
  if (!orderSegment || !orderSegment.present) {
    throw new Error(`ReplayIR does not cover tick ${tick}`);
  }
  const globalState = {};
  for (const [key, track] of Object.entries(replay.globalState)) {
    const segment = segmentAt(track, tick);
    if (segment && segment.present) globalState[key] = segment.value;
  }
  const byId = new Map();
  for (const entity of replay.entities) {
    if (!entityAliveAt(entity, tick)) continue;
    const object = {};
    for (const [key, segments] of Object.entries(entity.properties)) {
      const segment = segmentAt(segments, tick);
      if (segment && segment.present) object[key] = segment.value;
    }
    byId.set(entity.id, object);
  }
  const objects = orderSegment.value.map((id) => {
    const object = byId.get(id);
    if (!object) throw new Error(`ReplayIR object order references inactive entity ${id}`);
    return object;
  });
  const state = Object.assign(globalState, { objects });
  if (replay.tickFingerprints.length > 0) {
    const actualFingerprint = fingerprint(state);
    if (actualFingerprint !== replay.tickFingerprints[tick]) {
      throw new Error(`ReplayIR reconstruction fingerprint mismatch at tick ${tick}`);
    }
  }
  return state;
}

function reconstructVisualTick(replay, tick) {
  validateReplayIR(replay);
  tick = asNonnegativeInteger(tick, "tick");
  if (tick > replay.totalTicks) throw new RangeError("tick exceeds replay endpoint");
  const segment = segmentAt(replay.visualOverlay.states, tick);
  if (!segment || !segment.present) throw new Error(`ReplayIR visuals do not cover tick ${tick}`);
  return segment.value;
}

function decodeCalculationValue(value, track, segmentIndex) {
  const encoded = new Map(track[4]
    .filter(([index]) => index === segmentIndex)
    .map(([, pointer, code]) => [pointer, code]));
  const decode = (current, pointer) => {
    if (encoded.has(pointer)) {
      if (current !== null) throw new Error("non-finite calculation placeholder is not null");
      const code = encoded.get(pointer);
      return code === 0 ? Number.NaN
        : code === 1 ? Number.POSITIVE_INFINITY : Number.NEGATIVE_INFINITY;
    }
    if (Array.isArray(current)) {
      return current.map((child, index) => decode(child, `${pointer}/${index}`));
    }
    if (!current || typeof current !== "object") return current;
    return Object.fromEntries(Object.entries(current).map(([key, child]) => {
      const token = key.replaceAll("~", "~0").replaceAll("/", "~1");
      return [key, decode(child, `${pointer}/${token}`)];
    }));
  };
  return decode(value, "");
}

function reconstructReplayCalculations(replay, tick) {
  validateReplayIR(replay);
  tick = asNonnegativeInteger(tick, "tick");
  if (tick > replay.totalTicks) throw new RangeError("tick exceeds replay endpoint");
  if (!replay.calculationOutputs.enabled) {
    throw new Error("ReplayIR does not contain compiled calculation outputs");
  }
  const values = new Map();
  for (const entity of replay.entities) {
    if (!entityAliveAt(entity, tick)) continue;
    const calculations = {};
    for (const [key, track] of Object.entries(entity.calculations)) {
      const segment = segmentAt(track, tick);
      if (segment && segment.present) {
        calculations[key] = decodeCalculationValue(segment.value, track, segment.index);
      }
    }
    values.set(entity.id, calculations);
  }
  return values;
}

function reconstructRendererEvents(replay, tick) {
  validateReplayIR(replay);
  tick = asNonnegativeInteger(tick, "tick");
  if (tick > replay.totalTicks) throw new RangeError("tick exceeds replay");
  const {
    columns,
    entityIds,
    offsets,
    payloads,
    semanticIds,
  } = replay.rendererGraph;
  const events = [];
  for (let index = offsets[tick]; index < offsets[tick + 1]; index++) {
    events.push([
      tick,
      columns[0][index] < 0 ? null : entityIds[columns[0][index]],
      RENDERER_EVENT_OPS[columns[1][index]],
      columns[2][index] < 0 ? null : semanticIds[columns[2][index]],
      columns[3][index] < 0 ? null : payloads[columns[3][index]],
    ]);
  }
  return events;
}

function inventoryAction(action, actionTypes) {
  if (!action || typeof action !== "object") return;
  if (typeof action.action === "string") actionTypes.add(action.action);
  for (const value of Object.values(action)) {
    if (Array.isArray(value)) {
      for (const item of value) inventoryAction(item, actionTypes);
    } else if (value && typeof value === "object") {
      inventoryAction(value, actionTypes);
    }
  }
}

function normalizeRendererMetadata(value, path = "$", seen = new Set()) {
  if (value === null || typeof value !== "object") return value;
  if (seen.has(value)) throw new TypeError(`${path} contains a cyclic value`);
  seen.add(value);
  let result;
  if (Array.isArray(value)) {
    result = value.map((item, index) => normalizeRendererMetadata(
      item,
      `${path}[${index}]`,
      seen,
    ));
  } else {
    result = {};
    for (const key of Object.keys(value).sort()) {
      if (key === "_initialized") continue;
      const childPath = `${path}.${key}`;
      const child = value[key];
      result[key] = key === "id" && typeof child === "string" && /^id#\d+$/.test(child)
        ? `auto:${path}`
        : normalizeRendererMetadata(child, childPath, seen);
    }
  }
  seen.delete(value);
  return result;
}

function inventoryMetadataSemantics(value, inventory, path = "$", parentKey = "") {
  if (typeof value === "function") {
    inventory.functionSemantics.add(`${parentKey}:${fingerprint(
      Function.prototype.toString.call(value),
    )}`);
    return;
  }
  if (!value || typeof value !== "object") return;
  if (Array.isArray(value)) {
    for (let index = 0; index < value.length; index++) {
      inventoryMetadataSemantics(value[index], inventory, `${path}[${index}]`, parentKey);
    }
    return;
  }
  for (const [key, child] of Object.entries(value)) {
    if (key.startsWith("$")) inventory.expressionOperators.add(key);
    if (key === "method" && parentKey === "drawings" && typeof child === "string") {
      inventory.drawingMethods.add(child);
    }
    if (key === "id" && parentKey === "calculations" && typeof child === "string") {
      inventory.calculationIds.add(child);
    }
    inventoryMetadataSemantics(
      child,
      inventory,
      `${path}.${key}`,
      key === "method" ? parentKey : key,
    );
  }
}

function inventoryRendererMetadata(metadata) {
  if (!metadata || typeof metadata !== "object" || !metadata.objects) {
    throw new TypeError("renderer metadata with an objects map is required");
  }
  const processors = new Set();
  const actions = new Set();
  const semanticInventory = {
    calculationIds: new Set(),
    drawingMethods: new Set(),
    expressionOperators: new Set(),
    functionSemantics: new Set(),
  };
  inventoryMetadataSemantics(metadata, semanticInventory);
  for (const object of Object.values(metadata.objects)) {
    for (const processor of object.processors || []) {
      processors.add(processor.type || processor.name);
      for (const action of processor.actions || []) inventoryAction(action, actions);
    }
    if (object.disappearProcessor) {
      processors.add(object.disappearProcessor.type || object.disappearProcessor.name);
    }
    for (const actionGroup of object.actions || []) {
      for (const action of actionGroup.actions || []) inventoryAction(action, actions);
    }
  }
  return Object.freeze({
    objectTypes: Object.freeze(Object.keys(metadata.objects).sort()),
    processorTypes: Object.freeze([...processors].filter(Boolean).sort()),
    actionTypes: Object.freeze([...actions].sort()),
    preprocessors: Object.freeze([...(metadata.preprocessors || [])].sort()),
    calculationIds: Object.freeze([...semanticInventory.calculationIds].sort()),
    drawingMethods: Object.freeze([...semanticInventory.drawingMethods].sort()),
    expressionOperators: Object.freeze([...semanticInventory.expressionOperators].sort()),
    functionSemantics: Object.freeze([...semanticInventory.functionSemantics].sort()),
    layerIds: Object.freeze((metadata.layers || [])
      .map((layer) => layer && layer.id)
      .filter(Boolean)
      .sort()),
  });
}

function extractRendererWorldOptions(worldOptions) {
  if (!worldOptions || typeof worldOptions !== "object") return {};
  const extracted = {};
  for (const key of Object.keys(worldOptions).sort()) {
    if (RUNTIME_WORLD_OPTION_KEYS.has(key)) continue;
    extracted[key] = worldOptions[key];
  }
  // This intentionally remains strict: an unexpected non-serializable value
  // outside the known runtime infrastructure keys may affect a calculation and
  // must not be silently omitted.
  return canonicalize(extracted, "$.worldOptions");
}

function compileRendererContract(options) {
  if (!options || typeof options !== "object") throw new TypeError("options must be an object");
  if (options.decorations !== undefined && !Array.isArray(options.decorations)) {
    throw new TypeError("renderer decorations must be an array");
  }
  if (options.terrain !== undefined && !Array.isArray(options.terrain)) {
    throw new TypeError("renderer terrain must be an array");
  }
  const rendererImplementationFingerprint = options.rendererImplementationFingerprint === undefined
    || options.rendererImplementationFingerprint === null
    ? null
    : String(options.rendererImplementationFingerprint);
  if (rendererImplementationFingerprint !== null
    && !/^[0-9a-f]{64}$/.test(rendererImplementationFingerprint)) {
    throw new TypeError("rendererImplementationFingerprint must be a lowercase SHA-256 digest");
  }
  const normalizedMetadata = normalizeRendererMetadata(options.metadata);
  const inventory = inventoryRendererMetadata(normalizedMetadata);
  const functionSemantics = new Set(inventory.functionSemantics);
  if (options.worldOptions && typeof options.worldOptions.objectFilter === "function") {
    functionSemantics.add(`objectFilter:${fingerprint(
      Function.prototype.toString.call(options.worldOptions.objectFilter),
    )}`);
  }
  const contract = {
    schema: RENDERER_CONTRACT_SCHEMA,
    version: RENDERER_CONTRACT_VERSION,
    rendererVersion: options.rendererVersion || null,
    metadata: canonicalize(normalizedMetadata, "$.metadata"),
    resources: canonicalize(options.resources || {}, "$.resources"),
    decorations: canonicalize(options.decorations || [], "$.decorations"),
    terrain: canonicalize(options.terrain || [], "$.terrain"),
    worldOptions: extractRendererWorldOptions(options.worldOptions),
    inventory: Object.freeze({
      ...inventory,
      functionSemantics: Object.freeze([...functionSemantics].sort()),
      rendererImplementationFingerprints: Object.freeze(
        rendererImplementationFingerprint
          ? [rendererImplementationFingerprint]
          : [],
      ),
    }),
  };
  contract.fingerprint = fingerprintDeterministic(contract);
  const frozenContract = deepFreeze(contract);
  verifiedContracts.add(frozenContract);
  return frozenContract;
}

function unsupportedValues(actual, supported) {
  const allowed = new Set(supported || []);
  return actual.filter((value) => !allowed.has(value));
}

function assertRendererContractSupported(contract, support) {
  validateRendererContract(contract);
  if (!support || typeof support !== "object") {
    throw new TypeError("backend support manifest is required");
  }
  const failures = {};
  for (const [kind, values] of Object.entries(contract.inventory)) {
    if (!Array.isArray(values)) throw new Error(`invalid renderer inventory ${kind}`);
    failures[kind] = unsupportedValues(values, support[kind]);
  }
  const messages = Object.entries(failures)
    .filter(([, values]) => values.length > 0)
    .map(([kind, values]) => `${kind}: ${values.join(", ")}`);
  if (messages.length > 0) {
    throw new Error(`renderer contract is unsupported (${messages.join("; ")})`);
  }
  return true;
}

function validateTrack(track, totalTicks, name, requireCoverage = false, allowNonFinite = false) {
  if (!Array.isArray(track) || track.length !== 5) throw new Error(`invalid ${name} track`);
  const [bounds, values, absent, undefinedValues, nonFinite] = track;
  if (!Array.isArray(bounds) || !Array.isArray(values) || !Array.isArray(absent)
    || !Array.isArray(undefinedValues) || !Array.isArray(nonFinite)
    || bounds.length !== values.length * 2) {
    throw new Error(`invalid ${name} track columns`);
  }
  let previousEnd = -1;
  for (let index = 0; index < values.length; index++) {
    const startTick = bounds[index * 2];
    const endTick = bounds[index * 2 + 1];
    if (!Number.isSafeInteger(startTick) || !Number.isSafeInteger(endTick)
      || startTick < 0 || endTick <= startTick || endTick > totalTicks + 1
      || startTick < previousEnd
      || (requireCoverage && index > 0 && startTick !== previousEnd)) {
      throw new Error(`invalid ${name} segment ${index}`);
    }
    previousEnd = endTick;
  }
  if (requireCoverage && (values.length === 0
    || bounds[0] !== 0
    || bounds[bounds.length - 1] !== totalTicks + 1)) {
    throw new Error(`${name} track does not cover the replay`);
  }
  let previousAbsent = -1;
  for (const index of absent) {
    if (!Number.isSafeInteger(index) || index < 0 || index >= values.length
      || index <= previousAbsent) {
      throw new Error(`invalid ${name} absent index`);
    }
    previousAbsent = index;
  }
  let previousUndefined = -1;
  for (const index of undefinedValues) {
    if (!Number.isSafeInteger(index) || index < 0 || index >= values.length
      || index <= previousUndefined || sortedArrayIncludes(absent, index)) {
      throw new Error(`invalid ${name} undefined index`);
    }
    previousUndefined = index;
  }
  for (const value of values) assertStoredJSON(value, name);
  let previousIndex = -1;
  let segmentPointers = new Set();
  for (const entry of nonFinite) {
    if (!allowNonFinite || !Array.isArray(entry) || entry.length !== 3) {
      throw new Error(`invalid ${name} non-finite entry`);
    }
    const [index, pointer, code] = entry;
    if (!Number.isSafeInteger(index) || index < 0 || index >= values.length
      || typeof pointer !== "string" || (pointer !== "" && !pointer.startsWith("/"))
      || ![-1, 0, 1].includes(code)
      || index < previousIndex
      || sortedArrayIncludes(absent, index) || sortedArrayIncludes(undefinedValues, index)) {
      throw new Error(`invalid ${name} non-finite entry`);
    }
    if (index !== previousIndex) segmentPointers = new Set();
    if (segmentPointers.has(pointer)) throw new Error(`duplicate ${name} non-finite pointer`);
    segmentPointers.add(pointer);
    const tokens = pointer === "" ? [] : pointer.slice(1).split("/");
    if (tokens.some((token) => /~(?:[^01]|$)/.test(token))) {
      throw new Error(`invalid ${name} non-finite pointer`);
    }
    let target = values[index];
    let found = true;
    for (const encodedToken of tokens) {
      const token = encodedToken.replaceAll("~1", "/").replaceAll("~0", "~");
      if (Array.isArray(target)) {
        if (!/^(?:0|[1-9]\d*)$/.test(token)) {
          found = false;
          break;
        }
        const childIndex = Number(token);
        if (!Number.isSafeInteger(childIndex) || childIndex >= target.length) {
          found = false;
          break;
        }
        target = target[childIndex];
      } else if (target && typeof target === "object"
        && Object.prototype.hasOwnProperty.call(target, token)) {
        target = target[token];
      } else {
        found = false;
        break;
      }
    }
    if (!found || target !== null) {
      throw new Error(`invalid ${name} non-finite placeholder`);
    }
    previousIndex = index;
  }
}

function assertStoredJSON(value, name, seen = new Set()) {
  if (value === null || typeof value === "string" || typeof value === "boolean") return;
  if (typeof value === "number" && Number.isFinite(value)) return;
  if (!value || typeof value !== "object" || seen.has(value)) {
    throw new Error(`invalid ${name} value`);
  }
  seen.add(value);
  if (Array.isArray(value)) {
    for (const child of value) assertStoredJSON(child, name, seen);
  } else {
    for (const child of Object.values(value)) assertStoredJSON(child, name, seen);
  }
  seen.delete(value);
}

function fingerprintWithoutField(value) {
  const { fingerprint: _fingerprint, ...payload } = value;
  return fingerprintDeterministic(payload);
}

function validateReplayIR(replay, options = {}) {
  if (options.verifyFingerprint !== false && verifiedReplays.has(replay)) return true;
  if (!replay || replay.schema !== REPLAY_IR_SCHEMA || replay.version !== REPLAY_IR_VERSION) {
    throw new Error("unsupported ReplayIR schema/version");
  }
  if (!Number.isSafeInteger(replay.totalTicks) || replay.totalTicks < 0
    || !Array.isArray(replay.entities) || !Array.isArray(replay.tickFingerprints)
    || !replay.visualOverlay || typeof replay.visualOverlay.enabled !== "boolean"
    || !replay.calculationOutputs
    || typeof replay.calculationOutputs.enabled !== "boolean"
    || Object.keys(replay.calculationOutputs).join(",") !== "enabled"
    || !replay.rendererGraph
    || typeof replay.rendererGraph !== "object"
    || typeof replay.rendererGraph.enabled !== "boolean"
    || !Array.isArray(replay.rendererGraph.columns)
    || !Array.isArray(replay.rendererGraph.entityIds)
    || !Array.isArray(replay.rendererGraph.offsets)
    || !Array.isArray(replay.rendererGraph.payloads)
    || !Array.isArray(replay.rendererGraph.semanticIds)
    || Object.keys(replay.rendererGraph).sort().join(",")
      !== "columns,enabled,entityIds,offsets,payloads,semanticIds") {
    throw new Error("invalid ReplayIR structure");
  }
  if (!replay.timeline || typeof replay.timeline !== "object"
    || Array.isArray(replay.timeline)
    || Object.keys(replay.timeline).sort().join(",")
      !== "framesPerSecond,substepsPerSecond,tickTransitionSeconds,ticksPerSecond") {
    throw new Error("invalid ReplayIR timeline");
  }
  for (const [name, rate] of Object.entries(replay.timeline)) {
    if (rate !== null) normalizeRate(rate, `timeline.${name}`);
  }
  validateRenderConfig(replay.renderConfig);
  if (replay.randomSeed !== null && typeof replay.randomSeed !== "string") {
    throw new Error("invalid ReplayIR random seed");
  }
  try {
    if (normalizeRandomState(replay.randomStateAtFirstTick)
      !== replay.randomStateAtFirstTick) {
      throw new Error("non-canonical random state");
    }
  } catch (_) {
    throw new Error("invalid ReplayIR first-tick random state");
  }
  if (replay.rendererContractFingerprint !== null
    && !/^[0-9a-f]{64}$/.test(replay.rendererContractFingerprint)) {
    throw new Error("invalid ReplayIR renderer contract fingerprint");
  }
  if (!replay.globalState || typeof replay.globalState !== "object"
    || Array.isArray(replay.globalState)) {
    throw new Error("invalid ReplayIR globalState");
  }
  for (const [key, track] of Object.entries(replay.globalState)) {
    validateTrack(track, replay.totalTicks, `globalState.${key}`);
  }
  validateTrack(replay.objectOrder, replay.totalTicks, "objectOrder", true);
  validateTrack(replay.visualOverlay.states, replay.totalTicks, "visualOverlay", true);
  for (const order of replay.objectOrder[1]) {
    if (!Array.isArray(order) || order.some((id) => typeof id !== "string")
      || new Set(order).size !== order.length) {
      throw new Error("invalid ReplayIR object order value");
    }
  }
  const identities = new Set();
  for (const entity of replay.entities) {
    if (!entity || typeof entity.id !== "string" || identities.has(entity.id)
      || !Array.isArray(entity.lifetimes) || !entity.properties
      || typeof entity.properties !== "object" || Array.isArray(entity.properties)
      || !entity.calculations || typeof entity.calculations !== "object"
      || Array.isArray(entity.calculations)
      || Object.keys(entity).sort().join(",")
        !== "calculations,id,lifetimes,properties") {
      throw new Error("invalid ReplayIR entity");
    }
    identities.add(entity.id);
    let previousEnd = -1;
    for (const lifetime of entity.lifetimes) {
      if (!Array.isArray(lifetime) || lifetime.length !== 2
        || !Number.isSafeInteger(lifetime[0]) || !Number.isSafeInteger(lifetime[1])
        || lifetime[0] < 0 || lifetime[1] <= lifetime[0]
        || lifetime[1] > replay.totalTicks + 1 || lifetime[0] < previousEnd) {
        throw new Error(`invalid ReplayIR lifetime for ${entity.id}`);
      }
      previousEnd = lifetime[1];
    }
    for (const [key, track] of Object.entries(entity.properties)) {
      validateTrack(track, replay.totalTicks, `entity ${entity.id}.${key}`);
    }
    for (const [key, track] of Object.entries(entity.calculations)) {
      validateTrack(
        track,
        replay.totalTicks,
        `entity ${entity.id} calculation ${key}`,
        false,
        true,
      );
    }
    if (!replay.calculationOutputs.enabled
      && Object.keys(entity.calculations).length > 0) {
      throw new Error(`ReplayIR has disabled calculation outputs for ${entity.id}`);
    }
  }
  if (!Array.isArray(replay.actionEvents)) throw new Error("invalid ReplayIR actionEvents");
  let previousEventTick = -1;
  for (const event of replay.actionEvents) {
    if (!Array.isArray(event) || event.length !== 3
      || !Number.isSafeInteger(event[0]) || event[0] < previousEventTick
      || event[0] < 0 || event[0] > replay.totalTicks
      || !identities.has(event[1]) || typeof event[2] !== "string") {
      throw new Error("invalid ReplayIR action event");
    }
    previousEventTick = event[0];
  }
  const rendererOffsets = replay.rendererGraph.offsets;
  const rendererColumns = replay.rendererGraph.columns;
  const rendererEntityIds = replay.rendererGraph.entityIds;
  const rendererPayloads = replay.rendererGraph.payloads;
  const rendererSemanticIds = replay.rendererGraph.semanticIds;
  if (rendererColumns.length !== 4
    || rendererColumns.some((column) => !Array.isArray(column))
    || rendererColumns.some((column) => column.length !== rendererColumns[0].length)
    || rendererEntityIds.some((id) => typeof id !== "string" || !identities.has(id))
    || new Set(rendererEntityIds).size !== rendererEntityIds.length
    || rendererSemanticIds.some((id) => typeof id !== "string")
    || new Set(rendererSemanticIds).size !== rendererSemanticIds.length) {
    throw new Error("invalid ReplayIR renderer event columns");
  }
  const rendererEventCount = rendererColumns[0].length;
  if (rendererOffsets.length !== replay.totalTicks + 2
    || rendererOffsets[0] !== 0
    || rendererOffsets[rendererOffsets.length - 1] !== rendererEventCount
    || (!replay.rendererGraph.enabled && rendererEventCount !== 0)) {
    throw new Error("invalid ReplayIR renderer event index");
  }
  for (const payload of rendererPayloads) {
    assertStoredJSON(payload, "renderer event payload");
  }
  for (let index = 0; index < rendererEventCount; index++) {
    const entityIndex = rendererColumns[0][index];
    const opcode = rendererColumns[1][index];
    const semanticIndex = rendererColumns[2][index];
    const payloadIndex = rendererColumns[3][index];
    if (!Number.isSafeInteger(entityIndex)
      || entityIndex < -1 || entityIndex >= rendererEntityIds.length
      || !Number.isSafeInteger(opcode)
      || opcode < 0 || opcode >= RENDERER_EVENT_OPS.length
      || !Number.isSafeInteger(semanticIndex)
      || semanticIndex < -1 || semanticIndex >= rendererSemanticIds.length
      || !Number.isSafeInteger(payloadIndex)
      || payloadIndex < -1 || payloadIndex >= rendererPayloads.length) {
      throw new Error("invalid ReplayIR renderer event value");
    }
    if (!rendererEventShapeValid(
      entityIndex < 0 ? null : rendererEntityIds[entityIndex],
      RENDERER_EVENT_OPS[opcode],
      semanticIndex < 0 ? null : rendererSemanticIds[semanticIndex],
    )) {
      throw new Error("invalid ReplayIR renderer event shape");
    }
  }
  for (let tick = 0; tick <= replay.totalTicks; tick++) {
    const start = rendererOffsets[tick];
    const end = rendererOffsets[tick + 1];
    if (!Number.isSafeInteger(start) || !Number.isSafeInteger(end)
      || start < 0 || end < start || end > rendererEventCount) {
      throw new Error("invalid ReplayIR renderer event offset");
    }
  }
  if (replay.tickFingerprints.length !== 0
    && replay.tickFingerprints.length !== replay.totalTicks + 1) {
    throw new Error("invalid ReplayIR tick fingerprints");
  }
  for (const hash of replay.tickFingerprints) {
    if (!/^[0-9a-f]{64}$/.test(hash)) throw new Error("invalid ReplayIR tick fingerprint");
  }
  if (options.verifyFingerprint !== false
    && (!/^[0-9a-f]{64}$/.test(replay.fingerprint)
      || fingerprintWithoutField(replay) !== replay.fingerprint)) {
    throw new Error("ReplayIR fingerprint mismatch");
  }
  if (options.verifyFingerprint !== false) {
    deepFreeze(replay);
    verifiedReplays.add(replay);
  }
  return true;
}

function validateRendererContract(contract, options = {}) {
  if (options.verifyFingerprint !== false && verifiedContracts.has(contract)) return true;
  if (!contract
    || contract.schema !== RENDERER_CONTRACT_SCHEMA
    || contract.version !== RENDERER_CONTRACT_VERSION) {
    throw new Error("unsupported renderer contract schema/version");
  }
  if (!contract.inventory || typeof contract.inventory !== "object") {
    throw new Error("invalid renderer contract inventory");
  }
  if (!contract.metadata || typeof contract.metadata !== "object"
    || !contract.resources || typeof contract.resources !== "object"
    || !Array.isArray(contract.decorations)
    || !Array.isArray(contract.terrain)
    || !contract.worldOptions || typeof contract.worldOptions !== "object"
    || (contract.rendererVersion !== null && typeof contract.rendererVersion !== "string")
    || Object.keys(contract.inventory).sort().join(",") !== [...REQUIRED_INVENTORY_KEYS].sort().join(",")) {
    throw new Error("invalid renderer contract structure");
  }
  for (const [kind, values] of Object.entries(contract.inventory)) {
    if (!Array.isArray(values)
      || values.some((value) => typeof value !== "string")
      || values.some((value, index) => index > 0 && value <= values[index - 1])) {
      throw new Error(`invalid renderer contract inventory ${kind}`);
    }
  }
  if (options.verifyFingerprint !== false
    && (!/^[0-9a-f]{64}$/.test(contract.fingerprint)
      || fingerprintWithoutField(contract) !== contract.fingerprint)) {
    throw new Error("renderer contract fingerprint mismatch");
  }
  assertStoredJSON(contract.metadata, "renderer contract metadata");
  assertStoredJSON(contract.resources, "renderer contract resources");
  assertStoredJSON(contract.decorations, "renderer contract decorations");
  assertStoredJSON(contract.terrain, "renderer contract terrain");
  assertStoredJSON(contract.worldOptions, "renderer contract worldOptions");
  for (const implementation of contract.inventory.rendererImplementationFingerprints) {
    if (!/^[0-9a-f]{64}$/.test(implementation)) {
      throw new Error("invalid renderer implementation fingerprint");
    }
  }
  for (const semantic of contract.inventory.functionSemantics) {
    if (!/^[^:]+:[0-9a-f]{64}$/.test(semantic)) {
      throw new Error("invalid renderer function semantic");
    }
  }
  if (contract.inventory.functionSemantics
    .filter((semantic) => semantic.startsWith("objectFilter:")).length > 1) {
    throw new Error("renderer contract has multiple objectFilter semantics");
  }
  if (options.verifyFingerprint !== false) {
    deepFreeze(contract);
    verifiedContracts.add(contract);
  }
  return true;
}

function validateReplayArtifact(artifact) {
  if (!artifact || typeof artifact !== "object") throw new TypeError("artifact is required");
  validateRendererContract(artifact.rendererContract);
  validateReplayIR(artifact.replay);
  if (artifact.replay.rendererContractFingerprint !== artifact.rendererContract.fingerprint) {
    throw new Error("ReplayIR renderer contract fingerprint mismatch");
  }
  return true;
}

module.exports = {
  REPLAY_IR_SCHEMA,
  REPLAY_IR_VERSION,
  RENDERER_EVENT_OPS,
  RENDERER_CONTRACT_SCHEMA,
  RENDERER_CONTRACT_VERSION,
  assertRendererContractSupported,
  canonicalize,
  canonicalizeJSON,
  canonicalizeRendererEventValue,
  canonicalizeValue,
  compileRendererContract,
  compileReplayIR,
  extractRendererWorldOptions,
  fingerprint,
  inventoryRendererMetadata,
  normalizeRendererMetadata,
  reconstructReplayCalculations,
  reconstructRendererEvents,
  reconstructReplayTick,
  reconstructVisualTick,
  replayObjectId,
  stableStringify,
  validateRendererContract,
  validateReplayArtifact,
  validateReplayIR,
};
