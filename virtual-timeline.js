"use strict";

/**
 * A reduced rational backed by BigInt.
 *
 * Numbers are interpreted from their decimal string representation. Callers
 * should use a string such as "30000/1001" when a rate is fundamentally
 * fractional rather than decimal.
 */
class Rational {
  constructor(numerator, denominator = 1n) {
    numerator = BigInt(numerator);
    denominator = BigInt(denominator);

    if (denominator === 0n) {
      throw new RangeError("A rational denominator cannot be zero");
    }
    if (denominator < 0n) {
      numerator = -numerator;
      denominator = -denominator;
    }

    const divisor = greatestCommonDivisor(abs(numerator), denominator);
    this.numerator = numerator / divisor;
    this.denominator = denominator / divisor;
    Object.freeze(this);
  }

  static from(value, name = "value") {
    if (value instanceof Rational) return value;

    if (typeof value === "bigint") return new Rational(value);
    if (typeof value === "number") {
      if (!Number.isFinite(value)) {
        throw new TypeError(`${name} must be finite`);
      }
      return Rational.from(value.toString(), name);
    }
    if (typeof value === "string") {
      const parts = value.trim().split("/");
      if (parts.length > 2 || parts.some((part) => part.trim() === "")) {
        throw new TypeError(`${name} must be a number or rational`);
      }
      if (parts.length === 2) {
        return Rational.from(parts[0], name).divide(Rational.from(parts[1], name));
      }
      return fromDecimal(parts[0], name);
    }
    if (value && typeof value === "object") {
      const numerator = value.numerator ?? value.num;
      const denominator = value.denominator ?? value.den;
      if (numerator !== undefined && denominator !== undefined) {
        return new Rational(numerator, denominator);
      }
    }

    throw new TypeError(`${name} must be a number or rational`);
  }

  add(other) {
    other = Rational.from(other);
    return new Rational(
      this.numerator * other.denominator + other.numerator * this.denominator,
      this.denominator * other.denominator,
    );
  }

  subtract(other) {
    other = Rational.from(other);
    return new Rational(
      this.numerator * other.denominator - other.numerator * this.denominator,
      this.denominator * other.denominator,
    );
  }

  multiply(other) {
    other = Rational.from(other);
    return new Rational(
      this.numerator * other.numerator,
      this.denominator * other.denominator,
    );
  }

  divide(other) {
    other = Rational.from(other);
    if (other.numerator === 0n) throw new RangeError("Cannot divide by zero");
    return new Rational(
      this.numerator * other.denominator,
      this.denominator * other.numerator,
    );
  }

  compare(other) {
    other = Rational.from(other);
    const difference =
      this.numerator * other.denominator - other.numerator * this.denominator;
    return difference < 0n ? -1 : difference > 0n ? 1 : 0;
  }

  equals(other) {
    return this.compare(other) === 0;
  }

  floor() {
    if (this.numerator >= 0n) return this.numerator / this.denominator;
    return -((-this.numerator + this.denominator - 1n) / this.denominator);
  }

  ceil() {
    return -new Rational(-this.numerator, this.denominator).floor();
  }

  round() {
    if (this.numerator < 0n) return -new Rational(-this.numerator, this.denominator).round();
    return (2n * this.numerator + this.denominator) / (2n * this.denominator);
  }

  toNumber() {
    return Number(this.numerator) / Number(this.denominator);
  }

  toString() {
    return this.denominator === 1n
      ? this.numerator.toString()
      : `${this.numerator}/${this.denominator}`;
  }
}

/**
 * Generate the deterministic operations needed to render a video timeline.
 *
 * Event order is deliberately part of the API:
 *   apply tick 0, render frame 0, and immediately apply tick 1 as the first
 *   transition target. Tick N is advanced over [(N-1)/TPS, N/TPS]. At each
 *   boundary the next target is applied before a coincident render, except at
 *   the replay endpoint, where the completed final tick is rendered and no
 *   nonexistent tick is requested. Tick boundaries and a global fixed-substep
 *   grid both split advancement.
 */
function* createTimelineEvents(options) {
  const settings = normalizeOptions(options);
  const {
    frameCount,
    fullFrameCount,
    framesPerSecond,
    ticksPerSecond,
    substepsPerSecond,
    totalTicks,
  } = settings;
  const zero = new Rational(0n);
  const microsecondsPerSecond = new Rational(1_000_000n);
  const endpoint = new Rational(totalTicks).divide(ticksPerSecond);
  let time = zero;
  let tick = 0n;

  yield applyTickEvent(tick, time, microsecondsPerSecond);

  for (let frame = 0; frame < frameCount; frame += 1) {
    let frameTime = new Rational(BigInt(frame)).divide(framesPerSecond);
    if (frameCount === fullFrameCount && frame === frameCount - 1) {
      // A fractional number of frame intervals gets one short final interval,
      // placing the final render exactly at the replay endpoint.
      frameTime = endpoint;
    }

    while (time.compare(frameTime) < 0) {
      const nextTickTime = new Rational(tick).divide(ticksPerSecond);
      const completedSubsteps = time.multiply(substepsPerSecond).floor();
      const nextSubstepTime = new Rational(completedSubsteps + 1n).divide(
        substepsPerSecond,
      );
      const end = minimum(frameTime, nextTickTime, nextSubstepTime);
      const duration = end.subtract(time);

      yield Object.freeze({
        type: "advance",
        tick,
        from: time,
        to: end,
        duration,
        durationSeconds: duration.toNumber(),
      });
      time = end;

      if (time.equals(nextTickTime) && tick < totalTicks) {
        tick += 1n;
        yield applyTickEvent(tick, time, microsecondsPerSecond);
      }
    }

    let nextFrameTime = new Rational(BigInt(frame + 1)).divide(framesPerSecond);
    if (frame === frameCount - 1) {
      nextFrameTime = time.add(new Rational(1n).divide(framesPerSecond));
    } else if (frameCount === fullFrameCount && frame + 1 === frameCount - 1) {
      nextFrameTime = endpoint;
    }
    const timestampUs = time.multiply(microsecondsPerSecond).round();
    const nextTimestampUs = nextFrameTime.multiply(microsecondsPerSecond).round();
    yield Object.freeze({
      type: "render",
      frame,
      tick,
      time,
      timestampUs,
      durationUs: nextTimestampUs - timestampUs,
    });

    if (frame === 0 && totalTicks > 0n) {
      tick = 1n;
      yield applyTickEvent(tick, time, microsecondsPerSecond);
    }
  }

}

/**
 * Return the number of frames needed to include t=0 and the exact replay
 * endpoint. When the endpoint is off the regular frame grid, the final frame
 * uses a shorter interval.
 */
function calculateFrameCount(options) {
  if (!options || typeof options !== "object") {
    throw new TypeError("options must be an object");
  }
  const totalTicks = normalizeTotalTicks(options.totalTicks);
  const framesPerSecond = positiveRate(options.framesPerSecond, "framesPerSecond");
  const ticksPerSecond = positiveRate(options.ticksPerSecond, "ticksPerSecond");
  const frameIntervals = new Rational(totalTicks)
    .multiply(framesPerSecond)
    .divide(ticksPerSecond)
    .ceil();
  const frameCount = frameIntervals + 1n;
  if (frameCount > BigInt(Number.MAX_SAFE_INTEGER)) {
    throw new RangeError("calculated frameCount exceeds the safe integer range");
  }
  return Number(frameCount);
}

/** Run timeline hooks sequentially in the same order as createTimelineEvents. */
async function runVirtualTimeline(options, hooks) {
  if (!hooks || typeof hooks !== "object") {
    throw new TypeError("hooks must be an object");
  }
  for (const name of ["applyTick", "advance", "render"]) {
    if (typeof hooks[name] !== "function") {
      throw new TypeError(`hooks.${name} must be a function`);
    }
  }

  for (const event of createTimelineEvents(options)) {
    await hooks[event.type](event);
  }
}

function normalizeOptions(options) {
  if (!options || typeof options !== "object") {
    throw new TypeError("options must be an object");
  }
  const totalTicks = normalizeTotalTicks(options.totalTicks);
  const framesPerSecond = positiveRate(options.framesPerSecond, "framesPerSecond");
  const ticksPerSecond = positiveRate(options.ticksPerSecond, "ticksPerSecond");
  const fullFrameCount = calculateFrameCount({
    totalTicks,
    framesPerSecond,
    ticksPerSecond,
  });
  const frameCount = options.frameCount ?? fullFrameCount;
  if (!Number.isSafeInteger(frameCount) || frameCount < 1) {
    throw new RangeError("frameCount must be a positive safe integer");
  }
  if (frameCount > fullFrameCount) {
    throw new RangeError("frameCount cannot extend beyond the replay endpoint");
  }

  const substepsPerSecond = positiveRate(
    options.substepsPerSecond,
    "substepsPerSecond",
  );

  return {
    frameCount,
    framesPerSecond,
    fullFrameCount,
    substepsPerSecond,
    ticksPerSecond,
    totalTicks,
  };
}

function normalizeTotalTicks(value) {
  if (typeof value === "number") {
    if (!Number.isSafeInteger(value) || value < 0) {
      throw new RangeError("totalTicks must be a nonnegative safe integer or BigInt");
    }
    return BigInt(value);
  }
  if (typeof value === "bigint" && value >= 0n) return value;
  throw new RangeError("totalTicks must be a nonnegative safe integer or BigInt");
}

function positiveRate(value, name) {
  const rate = Rational.from(value, name);
  if (rate.numerator <= 0n) throw new RangeError(`${name} must be positive`);
  return rate;
}

function applyTickEvent(tick, time, microsecondsPerSecond) {
  return Object.freeze({
    type: "applyTick",
    tick,
    time,
    timestampUs: time.multiply(microsecondsPerSecond).round(),
  });
}

function minimum(...values) {
  return values.reduce((result, value) =>
    value.compare(result) < 0 ? value : result,
  );
}

function fromDecimal(value, name) {
  const match = /^([+-]?)(\d+)(?:\.(\d*))?(?:[eE]([+-]?\d+))?$/.exec(value);
  if (!match) throw new TypeError(`${name} must be a number or rational`);

  const [, sign, whole, fraction = "", exponentText = "0"] = match;
  const exponent = Number(exponentText);
  if (!Number.isSafeInteger(exponent)) {
    throw new RangeError(`${name} has an unsupported exponent`);
  }

  let numerator = BigInt(whole + fraction);
  let denominator = 10n ** BigInt(fraction.length);
  if (exponent > 0) numerator *= 10n ** BigInt(exponent);
  if (exponent < 0) denominator *= 10n ** BigInt(-exponent);
  if (sign === "-") numerator = -numerator;
  return new Rational(numerator, denominator);
}

function greatestCommonDivisor(a, b) {
  while (b !== 0n) [a, b] = [b, a % b];
  return a || 1n;
}

function abs(value) {
  return value < 0n ? -value : value;
}

module.exports = {
  Rational,
  calculateFrameCount,
  createTimelineEvents,
  runVirtualTimeline,
};
