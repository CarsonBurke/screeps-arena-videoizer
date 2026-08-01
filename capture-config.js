"use strict";

/**
 * Pure capture configuration resolution (options / URL params / env).
 * Independent of Pixi and WebCodecs so unit tests can lock precedence without
 * constructing a full capture session.
 */

const DEFAULT_ENCODER_QUEUE_LIMIT = 16;
const MAX_CAPTURE_DIMENSION = 8192;

function asPositiveNumber(value, fallback, label) {
  const number = value === undefined || value === null || value === ""
    ? fallback
    : Number(value);
  if (!Number.isFinite(number) || number <= 0) {
    throw new Error(`board capture: ${label} must be a positive number (got ${value})`);
  }
  return number;
}

function asPositiveInteger(value, fallback, label) {
  const number = asPositiveNumber(value, fallback, label);
  if (!Number.isInteger(number)) {
    throw new Error(`board capture: ${label} must be an integer (got ${value})`);
  }
  return number;
}

function asNonnegativeNumber(value, fallback, label) {
  const number = value === undefined || value === null || value === ""
    ? fallback
    : Number(value);
  if (!Number.isFinite(number) || number < 0) {
    throw new Error(`board capture: ${label} must be a nonnegative number (got ${value})`);
  }
  return number;
}

function asNonnegativeInteger(value, fallback, label) {
  const number = asNonnegativeNumber(value, fallback, label);
  if (!Number.isSafeInteger(number)) {
    throw new Error(`board capture: ${label} must be a safe integer (got ${value})`);
  }
  return number;
}

function asFiniteNumber(value, fallback, label) {
  const number = value === undefined || value === null || value === ""
    ? fallback
    : Number(value);
  if (!Number.isFinite(number)) {
    throw new Error(`board capture: ${label} must be finite (got ${value})`);
  }
  return number;
}

function asAutoZoom(value) {
  if (value === undefined || value === null || value === ""
    || String(value).trim().toLowerCase() === "auto") return "auto";
  return asPositiveNumber(value, null, "boardZoom");
}

function configuredValue(options, optionName, params, paramName, env, envName) {
  return options[optionName] ?? params.get(paramName) ?? env[envName];
}

function asCaptureDimension(value, fallback, label) {
  const number = asPositiveInteger(value, fallback, label);
  if (number > MAX_CAPTURE_DIMENSION) {
    throw new Error(
      `board capture: ${label} must be <= ${MAX_CAPTURE_DIMENSION} (got ${number})`,
    );
  }
  return number;
}

/**
 * @param {object} input
 * @param {object} input.options
 * @param {URLSearchParams} input.params
 * @param {NodeJS.ProcessEnv|object} input.env
 * @param {{ width?: number, height?: number }|null} [input.canvasHints]
 * @param {number|null} [input.totalTicksHint]
 * @returns {Readonly<object>}
 */
function resolveCaptureConfig({
  options = {},
  params = new URLSearchParams(),
  env = {},
  canvasHints = null,
  totalTicksHint = null,
} = {}) {
  const width = asCaptureDimension(
    configuredValue(options, "width", params, "capture-width", env, "SCREEPS_ARENA_BOARD_CAPTURE_WIDTH")
      || (canvasHints && canvasHints.width),
    null,
    "width",
  );
  const height = asCaptureDimension(
    configuredValue(options, "height", params, "capture-height", env, "SCREEPS_ARENA_BOARD_CAPTURE_HEIGHT")
      || (canvasHints && canvasHints.height),
    null,
    "height",
  );
  const fps = asPositiveNumber(
    configuredValue(options, "fps", params, "capture-fps", env, "SCREEPS_ARENA_BOARD_CAPTURE_FPS"),
    30,
    "fps",
  );
  const framesPerTick = asPositiveNumber(
    configuredValue(
      options,
      "framesPerTick",
      params,
      "capture-frames-per-tick",
      env,
      "SCREEPS_ARENA_BOARD_CAPTURE_FRAMES_PER_TICK",
    ),
    8,
    "framesPerTick",
  );
  const ticksPerSecond = asPositiveNumber(
    configuredValue(
      options,
      "ticksPerSecond",
      params,
      "capture-ticks-per-second",
      env,
      "SCREEPS_ARENA_BOARD_CAPTURE_TICKS_PER_SECOND",
    ),
    fps / framesPerTick,
    "ticksPerSecond",
  );
  const simulationFps = asPositiveNumber(
    configuredValue(
      options,
      "simulationFps",
      params,
      "capture-simulation-fps",
      env,
      "SCREEPS_ARENA_BOARD_CAPTURE_SIMULATION_FPS",
    ),
    60,
    "simulationFps",
  );
  const fixedStepSeconds = asPositiveNumber(
    options.fixedStepSeconds || env.SCREEPS_ARENA_BOARD_CAPTURE_FIXED_STEP_SECONDS,
    1 / simulationFps,
    "fixedStepSeconds",
  );
  const bitrate = asPositiveInteger(
    configuredValue(
      options,
      "bitrate",
      params,
      "capture-bitrate",
      env,
      "SCREEPS_ARENA_BOARD_CAPTURE_BITRATE",
    ),
    24_000_000,
    "bitrate",
  );
  const totalTicks = asNonnegativeInteger(
    options.totalTicks ?? totalTicksHint,
    null,
    "totalTicks",
  );
  const encoderQueueLimit = asPositiveInteger(
    options.encoderQueueLimit || env.SCREEPS_ARENA_BOARD_CAPTURE_ENCODER_QUEUE_LIMIT,
    DEFAULT_ENCODER_QUEUE_LIMIT,
    "encoderQueueLimit",
  );
  const tickDurationSeconds = asPositiveNumber(
    options.tickDurationSeconds,
    1 / ticksPerSecond,
    "tickDurationSeconds",
  );
  const boardZoom = asAutoZoom(configuredValue(
    options,
    "boardZoom",
    params,
    "board-zoom",
    env,
    "SCREEPS_ARENA_BOARD_CAPTURE_ZOOM",
  ));
  const boardPadding = asNonnegativeNumber(configuredValue(
    options,
    "boardPadding",
    params,
    "board-padding",
    env,
    "SCREEPS_ARENA_BOARD_CAPTURE_PADDING",
  ), 32, "boardPadding");
  const boardPanX = asFiniteNumber(configuredValue(
    options,
    "boardPanX",
    params,
    "board-pan-x",
    env,
    "SCREEPS_ARENA_BOARD_CAPTURE_PAN_X",
  ), 0, "boardPanX");
  const boardPanY = asFiniteNumber(configuredValue(
    options,
    "boardPanY",
    params,
    "board-pan-y",
    env,
    "SCREEPS_ARENA_BOARD_CAPTURE_PAN_Y",
  ), 0, "boardPanY");
  const compilerUnitTicks = asPositiveInteger(configuredValue(
    options,
    "compilerUnitTicks",
    params,
    "capture-compiler-unit-ticks",
    env,
    "SCREEPS_ARENA_BOARD_CAPTURE_COMPILER_UNIT_TICKS",
  ), 50, "compilerUnitTicks");
  const preloadConcurrency = asPositiveInteger(configuredValue(
    options,
    "preloadConcurrency",
    params,
    "capture-preload-concurrency",
    env,
    "SCREEPS_ARENA_BOARD_CAPTURE_PRELOAD_CONCURRENCY",
  ), 4, "preloadConcurrency");
  const replayIRSetting = configuredValue(
    options,
    "compileReplayIR",
    params,
    "capture-replay-ir",
    env,
    "SCREEPS_ARENA_BOARD_CAPTURE_REPLAY_IR",
  );
  const compileReplayIR = replayIRSetting === true
    || replayIRSetting === 1
    || ["1", "true"].includes(String(replayIRSetting || "").toLowerCase());

  return Object.freeze({
    width,
    height,
    fps,
    framesPerTick,
    ticksPerSecond,
    simulationFps,
    fixedStepSeconds,
    bitrate,
    totalTicks,
    encoderQueueLimit,
    tickDurationSeconds,
    boardZoom,
    boardPadding,
    boardPanX,
    boardPanY,
    compilerUnitTicks,
    preloadConcurrency,
    compileReplayIR,
  });
}

module.exports = {
  DEFAULT_ENCODER_QUEUE_LIMIT,
  MAX_CAPTURE_DIMENSION,
  asAutoZoom,
  asCaptureDimension,
  asFiniteNumber,
  asNonnegativeInteger,
  asNonnegativeNumber,
  asPositiveInteger,
  asPositiveNumber,
  configuredValue,
  resolveCaptureConfig,
};
