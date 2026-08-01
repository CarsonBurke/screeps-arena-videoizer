"use strict";

const {
  RendererCalculationEvaluator,
  createRendererRecord,
  mergeObjectMetadata,
  parseRendererValue,
  propsChanged,
  rendererRegistryEntries,
  shouldRun,
} = require("./renderer-calculations");
const {
  resolvePath,
} = require("./renderer-expressions");
const {
  RENDERER_EVENT_OPS: PROCESSOR_EVENT_OPS,
  replayObjectId,
} = require("./replay-ir");

const OBJECT_PROCESSOR_TYPES = new Set([
  "circle",
  "container",
  "draw",
  "resourceCircle",
  "road",
  "runAction",
  "say",
  "siteProgress",
  "sprite",
  "text",
  "userBadge",
]);

function assertExpressionRandomIndependent(value, label, seen = new Set()) {
  if (typeof value === "function") {
    if (/\bMath\s*\.\s*random\b/.test(Function.prototype.toString.call(value))) {
      throw new Error(`${label} cannot depend on random functions`);
    }
    return;
  }
  if (!value || typeof value !== "object" || seen.has(value)) return;
  seen.add(value);
  for (const [key, child] of Object.entries(value)) {
    if (key === "$random") {
      throw new Error(`${label} cannot depend on $random`);
    }
    assertExpressionRandomIndependent(child, label, seen);
  }
  seen.delete(value);
}

function assertScheduleRandomIndependent(metadata) {
  const conditionKeys = ["shouldRun", "until", "when"];
  const resultPayloadKeys = ["id", "parentId", "progress", "shouldCreate", "texture"];
  for (const [type, objectMetadata] of Object.entries(metadata.objects)) {
    for (const [kind, values] of [
      ["action", objectMetadata.actions || []],
      ["processor", objectMetadata.processors || []],
    ]) {
      for (let index = 0; index < values.length; index++) {
        const runnable = values[index];
        for (const key of conditionKeys) {
          if (runnable[key] !== undefined) {
            assertExpressionRandomIndependent(
              runnable[key],
              `renderer ${type} ${kind}[${index}].${key}`,
            );
          }
        }
        if (kind === "processor") {
          for (const key of resultPayloadKeys) {
            if (runnable.payload && runnable.payload[key] !== undefined) {
              assertExpressionRandomIndependent(
                runnable.payload[key],
                `renderer processor ${type}[${index}].payload.${key}`,
              );
            }
          }
        }
      }
    }
  }
}

/** @deprecated Prefer mergeObjectMetadata; kept as a stable processor export. */
const mergedObjectMetadata = mergeObjectMetadata;

function semanticId(metadata, path) {
  const id = metadata && metadata.id;
  return typeof id === "string" && !/^id#\d+$/.test(id)
    ? id
    : `auto:${path}`;
}

function definitionId(path) {
  return `auto:${path}`;
}

function shouldDestruct(metadata, stateParams, changed, random) {
  const when = metadata.when === undefined ? metadata.shouldRun : metadata.when;
  return changed && (
    (!metadata.until && when)
    || (metadata.until && parseRendererValue(metadata.until, stateParams, random))
  );
}

function payloadField(processor, name, params, random, fallback) {
  const payload = processor.payload || {};
  return payload[name] === undefined
    ? fallback
    : parseRendererValue(payload[name], params, random);
}

const DEFAULT_PROCESSOR_ID = Symbol("default processor id");

function processorResult(
  processor,
  params,
  record,
  scopeProcessorId,
  random,
) {
  const type = processor.type || processor.name;
  if (!OBJECT_PROCESSOR_TYPES.has(type)) return null;
  const configuredId = payloadField(
    processor,
    "id",
    params,
    random,
    DEFAULT_PROCESSOR_ID,
  );
  if (type === "runAction") {
    return configuredId !== DEFAULT_PROCESSOR_ID && configuredId
      ? record.scope[String(configuredId)]
        ? {
          node: record.scope[String(configuredId)],
          nodeId: null,
          touchesNode: false,
        }
        : null
      : { node: record.rootContainer, nodeId: null, touchesNode: false };
  }
  if (type === "resourceCircle") {
    const state = params.state || {};
    const prevState = params.prevState || {};
    const resourceType = state.resourceType || "energy";
    if (prevState[resourceType] === state[resourceType]) {
      return { node: null, nodeId: null, touchesNode: false };
    }
    return {
      node: {},
      nodeId: scopeProcessorId,
      touchesNode: true,
    };
  }
  if (type === "siteProgress") {
    const progress = payloadField(processor, "progress", params, random, undefined);
    if (record.oldProgress === progress) {
      return { node: null, nodeId: null, touchesNode: false };
    }
    record.oldProgress = progress;
    return {
      node: {},
      nodeId: scopeProcessorId,
      touchesNode: true,
    };
  }
  if (type === "road") {
    // The road adapter itself determines whether neighbor/decorations changed.
    // An early return retains its previous global scope entry.
    return record.scope[scopeProcessorId]
      ? { node: null, nodeId: null, touchesNode: false }
      : { node: {}, nodeId: scopeProcessorId, touchesNode: true };
  }
  if (type === "sprite") {
    const texture = payloadField(
      processor,
      "texture",
      params,
      random,
      params.objectMetadata.texture,
    );
    // The sprite wrapper returns before invoking object(), preserving the
    // previous scope entry, when no texture is configured.
    if (!texture) return { node: null, nodeId: null, touchesNode: false };
  }
  if (type === "userBadge") {
    // userBadge invokes sprite with a temporary scope. Its returned object is
    // owned through scope.processors but is not addressable as scope[id].
    return { node: {}, nodeId: null, touchesNode: false };
  }
  const nodeId = configuredId === DEFAULT_PROCESSOR_ID
    ? scopeProcessorId
    : String(configuredId);
  const parentId = payloadField(processor, "parentId", params, random, null);
  const hasParent = !parentId || !!record.scope[String(parentId)];
  const shouldCreate = payloadField(processor, "shouldCreate", params, random, true);
  return {
    node: hasParent && shouldCreate ? {} : null,
    nodeId,
    // Generic object() deletes scope[id] before either check.
    touchesNode: true,
  };
}

function ensureProcessorRecord(record) {
  if (!record.actions) record.actions = new Map();
  if (!record.processors) record.processors = new Map();
  if (!record.nodes) record.nodes = new Map();
  return record;
}

class RendererProcessorEvaluator {
  constructor(options) {
    if (!options || typeof options !== "object") {
      throw new TypeError("renderer processor options are required");
    }
    if (!options.metadata || typeof options.metadata !== "object"
      || !options.metadata.objects || typeof options.metadata.objects !== "object") {
      throw new TypeError("renderer metadata with an objects map is required");
    }
    this.metadata = options.metadata;
    this.world = options.world || { options: {} };
    this.random = options.random || Math.random;
    if (typeof this.random !== "function") throw new TypeError("random must be a function");
    assertScheduleRandomIndependent(this.metadata);
    this.getRandomState = options.getRandomState;
    this.setRandomState = options.setRandomState;
    if (typeof this.getRandomState !== "function"
      || typeof this.setRandomState !== "function") {
      throw new TypeError(
        "getRandomState and setRandomState are required for processor scheduling",
      );
    }
    if (options.records !== undefined && !(options.records instanceof Map)) {
      throw new TypeError("records must be a Map");
    }
    this.records = options.records || new Map();
  }

  evaluateTick(state, tick, tickDuration, calculations) {
    if (!(calculations instanceof Map)) {
      throw new TypeError("renderer processor calculations must be a Map");
    }
    return this.withRandomCheckpoint(() => {
      const prepared = this.prepareTick(state, tickDuration);
      return this.evaluatePrepared(prepared, tick, (entityId, record, objectState) => {
        if (!calculations.has(entityId)) {
          throw new Error(`renderer calculations are missing active object ${entityId}`);
        }
        const objectMetadata = mergedObjectMetadata(this.metadata, record.type);
        const prevState = record.state;
        const prevCalcs = record.calcs;
        const calcs = calculations.get(entityId);
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
        if (!prevState) {
          for (const [key, value] of Object.entries(objectMetadata.data || {})) {
            record.rootContainer[key] = parseRendererValue(
              value,
              stateParams,
              this.random,
            );
          }
        }
        return { calcs, objectMetadata, stateParams };
      });
    }).events;
  }

  evaluateFusedTick(state, tick, tickDuration, calculationEvaluator) {
    if (!(calculationEvaluator instanceof RendererCalculationEvaluator)
      || calculationEvaluator.records !== this.records) {
      throw new TypeError(
        "fused processor evaluation requires a calculation evaluator sharing records",
      );
    }
    return this.withRandomCheckpoint(() => {
      const prepared = calculationEvaluator.prepareTick(state, tickDuration);
      return this.evaluatePrepared(prepared, tick, (_entityId, record, objectState) => (
        calculationEvaluator.evaluateObject(record, objectState, prepared)
      ));
    });
  }

  withRandomCheckpoint(callback) {
    const randomStateBefore = this.getRandomState();
    let result;
    let failure;
    let randomStateAfter;
    try {
      result = callback();
    } catch (error) {
      failure = error;
    } finally {
      randomStateAfter = this.getRandomState();
      if (randomStateAfter !== randomStateBefore) {
        this.setRandomState(randomStateBefore);
      }
    }
    if (failure) throw failure;
    if (randomStateAfter !== randomStateBefore) {
      throw new Error(
        "renderer processor scheduling consumed hidden randomness; native replay would diverge",
      );
    }
    return result;
  }

  prepareTick(state, tickDuration) {
    if (!state || typeof state !== "object" || !Array.isArray(state.objects)) {
      throw new TypeError("renderer processor state must contain objects");
    }
    tickDuration = Number(tickDuration);
    if (!Number.isFinite(tickDuration) || tickDuration < 0) {
      throw new RangeError("tickDuration must be a nonnegative finite number");
    }
    const stateExtra = {
      ...state,
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
        "renderer objectFilter changes the scene and requires a compiled filter adapter",
      );
    }
    const statesById = new Map();
    for (const objectState of objects) {
      const id = replayObjectId(objectState);
      if (statesById.has(id)) {
        throw new Error(`duplicate object identity ${id} in renderer processors`);
      }
      statesById.set(id, objectState);
      let record = this.records.get(id);
      if (record && record.type !== objectState.type) {
        throw new Error(
          `renderer object ${id} changed type from ${record.type} to ${objectState.type}`,
        );
      }
      if (!record) {
        record = createRendererRecord(objectState.type);
        this.records.set(id, record);
      }
      ensureProcessorRecord(record);
    }
    return { objects, stateExtra, statesById, tickDuration };
  }

  evaluatePrepared(prepared, tick, evaluateObject) {
    if (!Number.isSafeInteger(tick) || tick < 0) {
      throw new RangeError("renderer processor tick must be a nonnegative safe integer");
    }
    const events = [];
    const calculationResults = new Map();
    for (const preprocessor of this.metadata.preprocessors || []) {
      events.push([tick, null, "preprocessor:run", String(preprocessor), null]);
    }

    // Match Object.values(world.gameObjects), including integer-like IDs.
    for (const [entityId, record] of rendererRegistryEntries(this.records)) {
      const objectState = prepared.statesById.get(entityId);
      if (!objectState) {
        // Disappear metadata and duration already live in the renderer
        // contract/timeline; the event is only the lifecycle boundary.
        events.push([tick, entityId, "object:remove", null, null]);
        this.records.delete(entityId);
        continue;
      }
      ensureProcessorRecord(record);
      const evaluated = evaluateObject(entityId, record, objectState);
      const { calcs, objectMetadata, stateParams } = evaluated;
      const prevState = record.state;
      const firstRun = !prevState;

      if (firstRun) {
        events.push([tick, entityId, "object:create", null, null]);
      }

      for (let index = 0; index < (objectMetadata.actions || []).length; index++) {
        const action = objectMetadata.actions[index];
        const path = `$.objects.${record.type}.actions[${index}]`;
        const id = definitionId(path);
        const targetId = action.targetId;
        if (targetId && !record.scope[targetId]) continue;
        const actionExists = record.actions.has(id);
        const changed = propsChanged(action, stateParams);
        const run = shouldRun(action, stateParams, changed, this.random);
        const onceAllow = !action.once || !actionExists;
        const destruct = !run && shouldDestruct(action, stateParams, changed, this.random);
        if (run && onceAllow) {
          if (actionExists) {
            events.push([tick, entityId, "action:finish", id, null]);
          }
          events.push([tick, entityId, "action:run", id, null]);
          record.actions.set(id, true);
        } else if (destruct) {
          if (actionExists) {
            events.push([tick, entityId, "action:finish", id, null]);
          }
          record.actions.delete(id);
        }
      }

      for (let index = 0; index < (objectMetadata.processors || []).length; index++) {
        const processor = objectMetadata.processors[index];
        const path = `$.objects.${record.type}.processors[${index}]`;
        const id = definitionId(path);
        const scopeId = semanticId(processor, path);
        const processorExists = record.processors.has(scopeId);
        const changed = propsChanged(processor, stateParams);
        const run = shouldRun(processor, stateParams, changed, this.random);
        const destruct = !run
          && shouldDestruct(processor, stateParams, changed, this.random);
        if (run || destruct) {
          const statePath = processor.path === undefined ? null : processor.path;
          const params = {
            ...stateParams,
            state: statePath === null ? objectState : resolvePath(objectState, statePath),
            prevState: statePath === null ? prevState : resolvePath(prevState, statePath),
            ...processor,
          };
          if (run && (!processor.once || !processorExists)) {
            const result = processorResult(
              processor,
              params,
              record,
              scopeId,
              this.random,
            );
            const nodeId = result && result.nodeId;
            const ownsNode = !!(result && result.node);
            if (result && result.touchesNode && nodeId) {
              record.nodes.delete(nodeId);
              delete record.scope[nodeId];
            }
            if (result && result.touchesNode && nodeId && result.node) {
              record.nodes.set(nodeId, result.node);
              record.scope[nodeId] = result.node;
            }
            events.push([tick, entityId, "processor:run", id, null]);
            record.processors.set(scopeId, { definitionId: id, nodeId, ownsNode });
            record.scope.processors[scopeId] = {};
          } else if (destruct) {
            events.push([tick, entityId, "processor:destruct", id, null]);
            // Official destructProcessor destroys its processor-owned result
            // but does not delete the global scope[id] pointer.
            record.processors.delete(scopeId);
            delete record.scope.processors[scopeId];
          }
        }
      }

      if (objectState.temp || objectState.tempRemove) {
        events.push([tick, entityId, "object:alpha", null, null]);
      }
      record.state = objectState;
      record.calcs = calcs;
      calculationResults.set(entityId, calcs);
    }
    return { calculations: calculationResults, events };
  }
}

class RendererGraphEvaluator {
  constructor(options) {
    if (!options || typeof options !== "object") {
      throw new TypeError("renderer graph options are required");
    }
    const records = new Map();
    this.calculations = new RendererCalculationEvaluator({
      ...options,
      records,
      rejectFunctionRandom: true,
    });
    this.processors = new RendererProcessorEvaluator({
      ...options,
      records,
    });
  }

  evaluateTick(state, tick, tickDuration) {
    return this.calculations.withRandomGuard(() => (
      this.processors.evaluateFusedTick(
        state,
        tick,
        tickDuration,
        this.calculations,
      )
    ));
  }
}

module.exports = {
  OBJECT_PROCESSOR_TYPES,
  PROCESSOR_EVENT_OPS,
  RendererGraphEvaluator,
  RendererProcessorEvaluator,
  definitionId,
  mergedObjectMetadata,
  semanticId,
};
