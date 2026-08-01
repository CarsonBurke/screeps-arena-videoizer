"use strict";

const { createRendererAction } = require("./renderer-actions");

const SUPPORTED_EXPRESSION_OPERATORS = Object.freeze([
  "$calc",
  "$processorParam",
  "$random",
  "$rel",
  "$state",
]);

function resolvePath(object, stringPath) {
  if (!object || stringPath === "^") return object;
  const path = String(stringPath)
    .replace(/\[(\w+)]/g, ".$1")
    .replace(/^\./, "")
    .split(".");
  let value = object;
  while (path.length > 0 && value) {
    const key = path.shift();
    if (key in value) value = value[key];
    else return undefined;
  }
  return value;
}

function scaledValue(value, options) {
  const defaultValue = options.default;
  const coefficient = options.koef === undefined ? 1 : options.koef;
  const resolved = value === undefined ? defaultValue : value;
  return typeof resolved === "number" ? coefficient * resolved : resolved;
}

const operators = {
  $calc(path, options, stateParams) {
    return scaledValue(resolvePath(stateParams.calcs, path), options);
  },
  $processorParam(path, options, stateParams) {
    return scaledValue(resolvePath(stateParams, path), options);
  },
  $random(range, _options, _stateParams, random) {
    return random() * range;
  },
  $rel(path, options, stateParams) {
    return scaledValue(resolvePath(stateParams.target || stateParams.renderData, path), options);
  },
  $state(path, options, stateParams) {
    return scaledValue(resolvePath(stateParams.state, path), options);
  },
};

function evaluateRendererExpression(expression, stateParams, random = Math.random) {
  if (typeof random !== "function") throw new TypeError("random must be a function");
  if (typeof expression === "function") return expression(stateParams);
  if (expression === null || expression === undefined) return expression;
  if (Array.isArray(expression)) {
    return expression.map((value) => evaluateRendererExpression(value, stateParams, random));
  }
  if (typeof expression !== "object") return expression;

  const operator = SUPPORTED_EXPRESSION_OPERATORS.find(
    (name) => Object.prototype.hasOwnProperty.call(expression, name),
  );
  if (operator) {
    const { [operator]: parameter, ...rest } = expression;
    return operators[operator](
      evaluateRendererExpression(parameter, stateParams, random),
      evaluateRendererExpression(rest, stateParams, random),
      stateParams,
      random,
    );
  }
  const unknownOperator = Object.keys(expression).find((key) => key.startsWith("$"));
  if (unknownOperator) {
    throw new Error(`unsupported renderer expression ${unknownOperator}`);
  }
  if (typeof expression.action === "string") {
    return createResolvedRendererAction(expression, stateParams, random);
  }

  const result = {};
  for (const [key, value] of Object.entries(expression)) {
    result[key] = evaluateRendererExpression(value, stateParams, random);
  }
  return result;
}

function createResolvedRendererAction(specification, stateParams, random = Math.random) {
  if (!specification || typeof specification !== "object"
    || typeof specification.action !== "string"
    || !Array.isArray(specification.params)) {
    throw new TypeError("renderer action specification is invalid");
  }
  return createRendererAction(resolveRendererActionSpecification(
    specification,
    stateParams,
    random,
  ));
}

function resolveRendererActionParameter(value, stateParams, random, options) {
  if (value && typeof value === "object" && typeof value.action === "string") {
    return resolveRendererActionSpecification(value, stateParams, random, options);
  }
  if (Array.isArray(value)) {
    return value.map(
      (child) => resolveRendererActionParameter(child, stateParams, random, options),
    );
  }
  if (!value || typeof value !== "object") {
    return evaluateRendererExpression(value, stateParams, random);
  }
  if (options.preserveRelative === true
    && Object.prototype.hasOwnProperty.call(value, "$rel")) {
    return value;
  }
  if (Object.keys(value).some((key) => key.startsWith("$"))) {
    return evaluateRendererExpression(value, stateParams, random);
  }
  const result = {};
  for (const [key, child] of Object.entries(value)) {
    result[key] = resolveRendererActionParameter(child, stateParams, random, options);
  }
  return result;
}

function resolveRendererActionSpecification(
  specification,
  stateParams,
  random = Math.random,
  options = {},
) {
  if (!specification || typeof specification !== "object"
    || typeof specification.action !== "string"
    || !Array.isArray(specification.params)) {
    throw new TypeError("renderer action specification is invalid");
  }
  return {
    action: specification.action,
    params: specification.params.map(
      (value) => resolveRendererActionParameter(value, stateParams, random, options),
    ),
  };
}

module.exports = {
  SUPPORTED_EXPRESSION_OPERATORS,
  createResolvedRendererAction,
  evaluateRendererExpression,
  resolveRendererActionSpecification,
  resolvePath,
};
