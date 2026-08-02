"use strict";

const assert = require("node:assert/strict");
const test = require("node:test");

const { applyBoardFrame, computeBoardFrame, enforceBoardFrame } = require("../board-framing");

test("auto framing contains the full square board inside pixel padding", () => {
  const frame = computeBoardFrame({
    width: 640,
    height: 640,
    boardWidth: 10_000,
    padding: 16,
    zoom: "auto",
  });
  assert.equal(frame.zoom, 0.0608);
  assert.equal(frame.x, 16);
  assert.equal(frame.y, 16);
  assert.equal(frame.width, 608);
  assert.equal(frame.height, 608);
  assert.deepEqual(
    [
      frame.outputWidth,
      frame.outputHeight,
      frame.boardWidth,
      frame.boardHeight,
      frame.worldMinX,
      frame.worldMinY,
      frame.pivotX,
      frame.pivotY,
    ],
    [640, 640, 10_000, 10_000, 0, 0, 0, 0],
  );
});

test("auto framing centers a square board in landscape output", () => {
  const frame = computeBoardFrame({
    width: 1920,
    height: 1080,
    boardWidth: 10_000,
    padding: 40,
  });
  assert.equal(frame.zoom, 0.1);
  assert.equal(frame.x, 460);
  assert.equal(frame.y, 40);
});

test("auto framing centers a square board in portrait output", () => {
  const frame = computeBoardFrame({
    width: 1080,
    height: 1920,
    boardWidth: 10_000,
    padding: 40,
  });
  assert.equal(frame.zoom, 0.1);
  assert.equal(frame.x, 40);
  assert.equal(frame.y, 460);
  assert.deepEqual([frame.left, frame.top, frame.right, frame.bottom], [40, 460, 1040, 1460]);
});

test("manual zoom remains centered before explicit pan offsets", () => {
  const frame = computeBoardFrame({
    width: 1920,
    height: 1080,
    boardWidth: 10_000,
    padding: 10,
    zoom: 0.08,
    panX: 25,
    panY: -10,
  });
  assert.equal(frame.x, 585);
  assert.equal(frame.y, 130);
});

test("applying a board frame updates Pixi and visual-observer transforms", () => {
  const updates = [];
  const stage = {
    scale: { set(value) { this.x = value; this.y = value; } },
    position: { set(x, y) { this.x = x; this.y = y; } },
  };
  const rendererRef = {
    _gameApp: {
      app: { stage, renderer: { emit(event) { updates.push(event); } } },
      world: { options: { VIEW_BOX: 10_000 } },
    },
    _zoomLevelSbj: { next(value) { updates.push(["zoom", value]); } },
    _positionSbj: { next(value) { updates.push(["position", value.x, value.y]); } },
  };
  const frame = applyBoardFrame({ screepsRendererRef: rendererRef }, {
    width: 2048,
    height: 2048,
    boardPadding: 32,
    boardZoom: "auto",
    boardPanX: 0,
    boardPanY: 0,
  });
  assert.equal(frame.zoom, 0.1984);
  assert.deepEqual([stage.position.x, stage.position.y], [32, 32]);
  assert.deepEqual(updates, ["_resized", ["zoom", 0.1984], ["position", 32, 32]]);
});

test("canonical half-cell terrain origin and Pixi pivot map to exact padded bounds", () => {
  const stage = {
    pivot: { x: -50, y: -50 },
    scale: { set(value) { this.x = value; this.y = value; } },
    position: { set(x, y) { this.x = x; this.y = y; } },
  };
  const frame = applyBoardFrame({
    screepsRendererRef: {
      _gameApp: {
        app: { stage, renderer: { emit() {} } },
        world: { options: { CELL_SIZE: 100, ROOM_SIZE: 100, VIEW_BOX: 10_000 } },
      },
    },
  }, {
    width: 2048,
    height: 2048,
    boardPadding: 32,
    boardZoom: "auto",
    boardPanX: 0,
    boardPanY: 0,
  });
  const screenMinX = stage.position.x + (-50 - stage.pivot.x) * stage.scale.x;
  const screenMaxX = stage.position.x + (9950 - stage.pivot.x) * stage.scale.x;
  assert.equal(screenMinX, 32);
  assert.equal(screenMaxX, 2016);
  assert.deepEqual([frame.left, frame.top, frame.right, frame.bottom], [32, 32, 2016, 2016]);
});

test("frame enforcement repairs camera drift before every render", () => {
  const stage = {
    scale: { x: 9, y: 8, set(x, y = x) { this.x = x; this.y = y; } },
    position: { x: -100, y: 700, set(x, y) { this.x = x; this.y = y; } },
  };
  const frame = computeBoardFrame({
    width: 2048,
    height: 2048,
    boardWidth: 10_000,
    padding: 32,
  });
  enforceBoardFrame(stage, frame);
  assert.deepEqual([stage.scale.x, stage.scale.y], [0.1984, 0.1984]);
  assert.deepEqual([stage.position.x, stage.position.y], [32, 32]);
});

test("invalid padding and zoom fail before touching the renderer", () => {
  assert.throws(() => computeBoardFrame({
    width: 100,
    height: 100,
    boardWidth: 1000,
    padding: 50,
  }), /padding/);
  assert.throws(() => computeBoardFrame({
    width: 100,
    height: 100,
    boardWidth: 1000,
    padding: 0,
    zoom: 0,
  }), /zoom/);
});
