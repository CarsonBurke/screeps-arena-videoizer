"use strict";

function finiteNumber(value, fallback, name) {
  const number = value === undefined || value === null || value === ""
    ? fallback
    : Number(value);
  if (!Number.isFinite(number)) throw new TypeError(`${name} must be finite`);
  return number;
}

function computeBoardFrame(options) {
  if (!options || typeof options !== "object") {
    throw new TypeError("board frame options must be an object");
  }
  const width = finiteNumber(options.width, NaN, "width");
  const height = finiteNumber(options.height, NaN, "height");
  const boardWidth = finiteNumber(options.boardWidth, NaN, "boardWidth");
  const boardHeight = finiteNumber(options.boardHeight, boardWidth, "boardHeight");
  const padding = finiteNumber(options.padding, 32, "padding");
  const panX = finiteNumber(options.panX, 0, "panX");
  const panY = finiteNumber(options.panY, 0, "panY");
  const worldMinX = finiteNumber(options.worldMinX, 0, "worldMinX");
  const worldMinY = finiteNumber(options.worldMinY, 0, "worldMinY");
  const pivotX = finiteNumber(options.pivotX, worldMinX, "pivotX");
  const pivotY = finiteNumber(options.pivotY, worldMinY, "pivotY");
  if (width <= 0 || height <= 0 || boardWidth <= 0 || boardHeight <= 0) {
    throw new RangeError("capture and board dimensions must be positive");
  }
  if (padding < 0 || padding * 2 >= width || padding * 2 >= height) {
    throw new RangeError("padding must leave a positive capture area");
  }

  const requestedZoom = options.zoom;
  const auto = requestedZoom === undefined
    || requestedZoom === null
    || requestedZoom === ""
    || requestedZoom === "auto";
  const zoom = auto
    ? Math.min((width - 2 * padding) / boardWidth, (height - 2 * padding) / boardHeight)
    : finiteNumber(requestedZoom, NaN, "zoom");
  if (zoom <= 0) throw new RangeError("zoom must be positive or 'auto'");

  const renderedWidth = boardWidth * zoom;
  const renderedHeight = boardHeight * zoom;
  const left = (width - renderedWidth) / 2 + panX;
  const top = (height - renderedHeight) / 2 + panY;
  return Object.freeze({
    mode: auto ? "auto" : "manual",
    outputWidth: width,
    outputHeight: height,
    boardWidth,
    boardHeight,
    worldMinX,
    worldMinY,
    pivotX,
    pivotY,
    zoom,
    // Pixi applies position + (local - pivot) * scale. Compute the stage
    // position from the authoritative world origin instead of assuming either
    // the terrain or the stage begins at zero.
    x: left - (worldMinX - pivotX) * zoom,
    y: top - (worldMinY - pivotY) * zoom,
    left,
    top,
    right: left + renderedWidth,
    bottom: top + renderedHeight,
    width: renderedWidth,
    height: renderedHeight,
    padding,
    panX,
    panY,
  });
}

function applyBoardFrame(component, config) {
  const rendererRef = component && component.screepsRendererRef;
  const gameApp = rendererRef && rendererRef._gameApp;
  const app = gameApp && gameApp.app;
  const stage = app && app.stage;
  const worldOptions = gameApp && gameApp.world && gameApp.world.options
    || component && component.worldConfig
    || {};
  if (!stage) throw new Error("board framing: renderer stage is unavailable");

  const boardWidth = Number(worldOptions.VIEW_BOX)
    || Number(worldOptions.CELL_SIZE) * Number(worldOptions.ROOM_SIZE);
  const boardHeight = Number(worldOptions.VIEW_BOX_HEIGHT) || boardWidth;
  const cellSize = Number(worldOptions.CELL_SIZE) || 0;
  const worldMinX = Number.isFinite(Number(worldOptions.WORLD_MIN_X))
    ? Number(worldOptions.WORLD_MIN_X)
    : -cellSize / 2;
  const worldMinY = Number.isFinite(Number(worldOptions.WORLD_MIN_Y))
    ? Number(worldOptions.WORLD_MIN_Y)
    : -cellSize / 2;
  const frame = computeBoardFrame({
    width: config.width,
    height: config.height,
    boardWidth,
    boardHeight,
    padding: config.boardPadding,
    zoom: config.boardZoom,
    panX: config.boardPanX,
    panY: config.boardPanY,
    worldMinX,
    worldMinY,
    pivotX: stage.pivot && stage.pivot.x,
    pivotY: stage.pivot && stage.pivot.y,
  });

  enforceBoardFrame(stage, frame);
  if (app.renderer && typeof app.renderer.emit === "function") app.renderer.emit("_resized");

  // Keep Canvas2D visuals and any remaining renderer observers aligned with the
  // direct stage transform used by capture.
  if (rendererRef._zoomLevelSbj && typeof rendererRef._zoomLevelSbj.next === "function") {
    rendererRef._zoomLevelSbj.next(frame.zoom);
  }
  if (rendererRef._positionSbj && typeof rendererRef._positionSbj.next === "function") {
    rendererRef._positionSbj.next(stage.position);
  }
  return frame;
}

function enforceBoardFrame(stage, frame) {
  if (!stage || !frame) throw new TypeError("stage and frame are required");
  if (stage.scale && typeof stage.scale.set === "function") stage.scale.set(frame.zoom);
  else {
    stage.scale.x = frame.zoom;
    stage.scale.y = frame.zoom;
  }
  if (stage.position && typeof stage.position.set === "function") {
    stage.position.set(frame.x, frame.y);
  } else {
    stage.position.x = frame.x;
    stage.position.y = frame.y;
  }
  return frame;
}

module.exports = { applyBoardFrame, computeBoardFrame, enforceBoardFrame };
