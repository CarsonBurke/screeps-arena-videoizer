"use strict";

const SUPPORTED_ACTION_TYPES = Object.freeze([
  "AlphaTo",
  "DelayTime",
  "Ease",
  "FadeIn",
  "FadeOut",
  "FilterTo",
  "MoveTo",
  "Repeat",
  "RotateBy",
  "RotateTo",
  "ScaleTo",
  "Sequence",
  "Spawn",
  "TintTo",
]);

class Action {
  reset() {}

  update() {
    throw new Error("action update is not implemented");
  }

  finish() {
    this.reset();
  }
}

class TimeableAction extends Action {
  constructor(timeSeconds) {
    super();
    this.time = Number(timeSeconds) * 1000;
    if (!Number.isFinite(this.time) || this.time < 0) {
      throw new RangeError("action time must be a nonnegative finite number");
    }
  }

  reset() {
    this.restTime = this.time;
  }

  update(container, deltaMs) {
    this.restTime -= deltaMs;
    if (this.restTime <= 0) {
      this.finish(container);
      return true;
    }
    return false;
  }
}

function interpolateScalar(action, current, target, deltaMs) {
  return current + (target - current) / action.restTime * deltaMs;
}

class AlphaTo extends TimeableAction {
  constructor(alpha, time) {
    super(time);
    this.alpha = alpha;
    this.reset();
  }

  update(container, deltaMs) {
    container.alpha = interpolateScalar(this, container.alpha, this.alpha, deltaMs);
    return super.update(container, deltaMs);
  }

  finish(container) {
    container.alpha = this.alpha;
    super.finish(container);
  }
}

class DelayTime extends TimeableAction {
  constructor(time) {
    super(time);
    this.reset();
  }
}

class FadeIn extends TimeableAction {
  constructor(time) {
    super(time);
    this.alpha = 1;
    this.reset();
  }

  update(container, deltaMs) {
    container.alpha = interpolateScalar(this, container.alpha, this.alpha, deltaMs);
    return super.update(container, deltaMs);
  }

  // The official implementation deliberately does not call super.finish().
  finish(container) {
    container.alpha = this.alpha;
  }
}

class FadeOut extends FadeIn {
  constructor(time) {
    super(time);
    this.alpha = 0;
    this.reset();
  }
}

class FilterTo extends TimeableAction {
  constructor(filterIndex, propertyName, propertyValue, time) {
    super(time);
    this.filterIdx = filterIndex;
    this.propName = propertyName;
    this.propValue = propertyValue;
    this.reset();
  }

  update(container, deltaMs) {
    const filter = container.filters[this.filterIdx];
    filter[this.propName] = interpolateScalar(
      this,
      filter[this.propName],
      this.propValue,
      deltaMs,
    );
    return super.update(container, deltaMs);
  }

  finish(container) {
    container.filters[this.filterIdx][this.propName] = this.propValue;
    super.finish(container);
  }
}

class MoveTo extends TimeableAction {
  constructor(x, y, time) {
    super(time);
    this.x = x;
    this.y = y;
    this.reset();
  }

  update(container, deltaMs) {
    const position = container.position;
    container.x += (this.x - position.x) / this.restTime * deltaMs;
    container.y += (this.y - position.y) / this.restTime * deltaMs;
    return super.update(container, deltaMs);
  }

  finish(container) {
    container.x = this.x;
    container.y = this.y;
    super.finish(container);
  }
}

class RotateBy extends TimeableAction {
  constructor(rotation, time) {
    super(time);
    this.rotation = rotation;
    this.reset();
  }

  reset() {
    super.reset();
    this.trotation = null;
  }

  update(container, deltaMs) {
    const rotation = container.rotation;
    if (this.trotation === null) this.trotation = rotation + this.rotation;
    container.rotation += (this.trotation - rotation) / this.restTime * deltaMs;
    return super.update(container, deltaMs);
  }

  finish(container) {
    container.rotation = this.trotation;
    super.finish(container);
  }
}

class RotateTo extends TimeableAction {
  constructor(rotation, time) {
    super(time);
    this.rotation = rotation;
    this.reset();
  }

  update(container, deltaMs) {
    const rotation = container.rotation;
    while (this.rotation - rotation > Math.PI) this.rotation -= Math.PI * 2;
    while (this.rotation - rotation < -Math.PI) this.rotation += Math.PI * 2;
    container.rotation += (this.rotation - rotation) / this.restTime * deltaMs;
    return super.update(container, deltaMs);
  }

  finish(container) {
    container.rotation = this.rotation;
    super.finish(container);
  }
}

class ScaleTo extends TimeableAction {
  constructor(x, y, time) {
    super(time);
    this.x = x;
    this.y = y;
    this.reset();
  }

  update(container, deltaMs) {
    container.scale.x += (this.x - container.scale.x) / this.restTime * deltaMs;
    container.scale.y += (this.y - container.scale.y) / this.restTime * deltaMs;
    return super.update(container, deltaMs);
  }

  finish(container) {
    container.scale.x = this.x;
    container.scale.y = this.y;
    super.finish(container);
  }
}

function colorComponent(color, shift) {
  return (color >>> shift) & 0xff;
}

function packColor(red, green, blue) {
  const clamp = (value) => Math.min(255, Math.max(0, Math.floor(value)));
  return (clamp(red) << 16) | (clamp(green) << 8) | clamp(blue);
}

function validColor(color) {
  return typeof color === "number"
    ? Math.min(0xffffff, Math.max(0, Math.floor(color)))
    : color;
}

class TintTo extends TimeableAction {
  constructor(tint, time) {
    super(time);
    this.tint = validColor(tint);
    this.reset();
  }

  update(container, deltaMs) {
    const from = container.tint;
    const target = this.tint;
    container.tint = packColor(
      interpolateScalar(this, colorComponent(from, 16), colorComponent(target, 16), deltaMs),
      interpolateScalar(this, colorComponent(from, 8), colorComponent(target, 8), deltaMs),
      interpolateScalar(this, colorComponent(from, 0), colorComponent(target, 0), deltaMs),
    );
    return super.update(container, deltaMs);
  }

  finish(container) {
    container.tint = validColor(this.tint);
    super.finish(container);
  }
}

class Sequence extends Action {
  constructor(actions) {
    super();
    this.actions = actions;
    this.reset();
  }

  reset() {
    this.index = 0;
  }

  update(container, deltaMs) {
    if (this.index >= this.actions.length) return true;
    const action = this.actions[this.index];
    if (action.update(container, deltaMs)) {
      action.reset();
      this.index++;
    }
    return false;
  }
}

class Spawn extends Action {
  constructor(actions) {
    super();
    this.actions = actions;
    this.reset();
  }

  reset() {
    this.actionsToRun = [...this.actions];
  }

  update(container, deltaMs) {
    this.actionsToRun = this.actionsToRun.filter((action) => {
      if (!action.update(container, deltaMs)) return true;
      action.reset();
      return false;
    });
    if (this.actionsToRun.length === 0) {
      this.finish(container);
      return true;
    }
    return false;
  }
}

class Repeat extends Action {
  constructor(action, count) {
    super();
    this.action = action;
    this.count = count;
    this.reset();
  }

  reset() {
    this.remaining = this.count || Infinity;
  }

  update(container, deltaMs) {
    if (this.action.update(container, deltaMs)) {
      this.action.reset();
      this.remaining--;
    }
    if (this.remaining <= 0) {
      this.finish(container);
      return true;
    }
    return false;
  }
}

const easing = Object.freeze({
  LINEAR: (time) => time,
  EASE_IN_QUAD: (time) => time ** 2,
  EASE_OUT_QUAD: (time) => 1 - Math.abs((time - 1) ** 2),
  EASE_IN_OUT_QUAD: (time) => time < 0.5
    ? 0.5 * (time * 2) ** 2
    : 1 - 0.5 * Math.abs((time * 2 - 2) ** 2),
  EASE_IN_CUBIC: (time) => time ** 3,
  EASE_OUT_CUBIC: (time) => 1 - Math.abs((time - 1) ** 3),
  EASE_IN_OUT_CUBIC: (time) => time < 0.5
    ? 0.5 * (time * 2) ** 3
    : 1 - 0.5 * Math.abs((time * 2 - 2) ** 3),
  EASE_IN_QUART: (time) => time ** 4,
  EASE_OUT_QUART: (time) => 1 - Math.abs((time - 1) ** 4),
  EASE_IN_OUT_QUART: (time) => time < 0.5
    ? 0.5 * (time * 2) ** 4
    : 1 - 0.5 * Math.abs((time * 2 - 2) ** 4),
  EASE_IN_QUINT: (time) => time ** 5,
  EASE_OUT_QUINT: (time) => 1 - Math.abs((time - 1) ** 5),
  EASE_IN_OUT_QUINT: (time) => time < 0.5
    ? 0.5 * (time * 2) ** 5
    : 1 - 0.5 * Math.abs((time * 2 - 2) ** 5),
});

class Ease extends Action {
  constructor(action, easeType = easing.EASE_OUT_QUAD) {
    super();
    this.time = action.time;
    this.action = action;
    this.easeType = typeof easeType === "string" ? easing[easeType] : easeType;
    if (typeof this.easeType !== "function") throw new Error(`wrong easeType ${easeType}`);
    this.reset();
  }

  reset() {
    this.originalTimePassed = 0;
    this.timePassed = 0;
    this.action.reset();
  }

  update(container, deltaMs) {
    this.originalTimePassed += deltaMs;
    const easeDelta = this.originalTimePassed <= this.time
      ? Math.max(
        this.time * this.easeType(this.originalTimePassed / this.time) - this.timePassed,
        0,
      )
      : deltaMs;
    this.timePassed += easeDelta;
    const result = this.action.update(container, easeDelta);
    if (result) this.finish(container);
    return result;
  }

  finish(container) {
    this.action.finish(container);
    super.finish(container);
  }
}

const ACTION_CLASSES = Object.freeze({
  AlphaTo,
  DelayTime,
  Ease,
  FadeIn,
  FadeOut,
  FilterTo,
  MoveTo,
  Repeat,
  RotateBy,
  RotateTo,
  ScaleTo,
  Sequence,
  Spawn,
  TintTo,
});

function instantiateParameter(value) {
  if (Array.isArray(value)) return value.map(instantiateParameter);
  if (value && typeof value === "object") {
    if (typeof value.action === "string") return createRendererAction(value);
    const result = {};
    for (const [key, child] of Object.entries(value)) {
      if (key.startsWith("$")) {
        throw new Error("renderer action expressions must be resolved before instantiation");
      }
      result[key] = instantiateParameter(child);
    }
    return result;
  }
  return value;
}

function createRendererAction(specification) {
  if (!specification || typeof specification !== "object"
    || typeof specification.action !== "string"
    || !Array.isArray(specification.params)) {
    throw new TypeError("renderer action specification is invalid");
  }
  const ActionClass = ACTION_CLASSES[specification.action];
  if (!ActionClass) throw new Error(`unsupported renderer action ${specification.action}`);
  return new ActionClass(...specification.params.map(instantiateParameter));
}

function advanceRendererAction(action, container, deltaMs) {
  if (!action || typeof action.update !== "function") throw new TypeError("action is invalid");
  deltaMs = Number(deltaMs);
  if (!Number.isFinite(deltaMs) || deltaMs < 0) {
    throw new RangeError("deltaMs must be a nonnegative finite number");
  }
  return action.update(container, deltaMs);
}

module.exports = {
  SUPPORTED_ACTION_TYPES,
  advanceRendererAction,
  createRendererAction,
  easing,
};
