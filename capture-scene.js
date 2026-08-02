"use strict";

const { preserveRendererRandomState } = require("./capture-rng");

function resetAnimatedSpritePhase(root) {
  let reset = 0;
  const pending = [root];
  while (pending.length > 0) {
    const node = pending.pop();
    if (!node) continue;
    if (Array.isArray(node.children)) pending.push(...node.children);
    if (!("_currentTime" in node) || typeof node.gotoAndStop !== "function") continue;
    const wasPlaying = !!node.playing;
    node.gotoAndStop(0);
    if (wasPlaying && typeof node.play === "function") node.play();
    reset++;
  }
  const swampObjects = root && root.terrainObjects && root.terrainObjects.swampObjects;
  if (Array.isArray(swampObjects)) {
    const resetTilePositions = new Set();
    for (const index of [3, 4]) {
      const tilePosition = swampObjects[index] && swampObjects[index].tilePosition;
      if (!tilePosition || resetTilePositions.has(tilePosition)) continue;
      resetTilePositions.add(tilePosition);
      if (typeof tilePosition.set === "function") tilePosition.set(0, 0);
      else {
        tilePosition.x = 0;
        tilePosition.y = 0;
      }
      reset++;
    }
  }
  return reset;
}

function collectDisplayTree(root, output) {
  if (!root || typeof root !== "object" || output.has(root)) return;
  output.add(root);
  if (Array.isArray(root.children)) {
    for (const child of root.children) collectDisplayTree(child, output);
  }
}

function collectDisplayObjects(value, output, visited = new Set()) {
  if (!value || typeof value !== "object" || visited.has(value)) return;
  visited.add(value);
  const prototype = Object.getPrototypeOf(value);
  const isPlainRecord = prototype === Object.prototype || prototype === null;
  if (Array.isArray(value) || isPlainRecord) {
    for (const child of Object.values(value)) collectDisplayObjects(child, output, visited);
    return;
  }
  collectDisplayTree(value, output);
}

function displayObjectIsDestroyed(value) {
  return !value
    || value.destroyed === true
    || value._destroyed === true;
}

function destroyTransientDisplayObject(value) {
  if (displayObjectIsDestroyed(value) || typeof value.destroy !== "function") return false;
  value.destroy({ children: true, texture: false, baseTexture: false });
  return true;
}

/**
 * Return the live renderer to the state immediately before replay tick 0.
 *
 * The replay component may have rendered an arbitrary tick before capture
 * acquires its clocks. Calling World.removeAllObjects() is insufficient because
 * disappear processors intentionally leave actions alive. The official private
 * GameObject teardown is the only synchronous path which destructs processors,
 * cancels owned actions, and removes the root without playing that animation.
 */
function resetRendererScene(gameApp) {
  const world = gameApp && gameApp.world;
  const stage = world && world.stage || gameApp && gameApp.app && gameApp.app.stage;
  const actionManager = gameApp && gameApp.actionManager
    || stage && stage.actionManager;
  if (!world || !world.gameObjects || typeof world.gameObjects !== "object") {
    throw new Error("board capture: renderer world object registry is unavailable");
  }
  if (!actionManager || !actionManager.actions || typeof actionManager.actions !== "object") {
    throw new Error("board capture: renderer action registry is unavailable");
  }

  return preserveRendererRandomState(() => {
    const gameObjects = Object.values(world.gameObjects);
    const actionRecords = Object.values(actionManager.actions);
    const objectNodes = new Set();
    const decorationNodes = new Set();
    const persistentNodes = new Set();
    for (const gameObject of gameObjects) {
      collectDisplayTree(gameObject && gameObject.rootContainer, objectNodes);
    }
    collectDisplayTree(world.decorationsContainer, decorationNodes);
    collectDisplayObjects(world.layers, persistentNodes);
    collectDisplayObjects(stage && stage.terrainObjects, persistentNodes);

    for (const gameObject of gameObjects) {
      if (!gameObject || typeof gameObject._destroy !== "function") {
        throw new Error("board capture: renderer GameObject synchronous teardown is unavailable");
      }
      gameObject._destroy();
    }
    for (const id of Object.keys(world.gameObjects)) delete world.gameObjects[id];

    let transientActionContainers = 0;
    const handledContainers = new Set();
    for (const record of actionRecords) {
      const container = record && record.container;
      if (!container || handledContainers.has(container)) continue;
      handledContainers.add(container);
      if (objectNodes.has(container)
        || decorationNodes.has(container)
        || persistentNodes.has(container)
        || container === stage) {
        continue;
      }
      if (destroyTransientDisplayObject(container)) transientActionContainers++;
    }

    actionManager.actions = {};
    actionManager._actionsToDelete = [];
    actionManager._last = 0;

    const decorations = Array.isArray(world.decorations) ? world.decorations : [];
    const shouldRebuildDecorations = !!world.decorationsContainer || decorations.length > 0;
    if (shouldRebuildDecorations) {
      if (typeof gameApp.setDecorations !== "function") {
        throw new Error("board capture: renderer decoration reset API is unavailable");
      }
      gameApp.setDecorations(decorations);
    }

    return {
      gameObjects: gameObjects.length,
      actions: actionRecords.length,
      transientActionContainers,
      decorationsRebuilt: shouldRebuildDecorations,
    };
  });
}

function retainedTerrainObjects(world) {
  const terrainObjects = world && world.stage && world.stage.terrainObjects;
  if (!terrainObjects) return [];
  const walls = Array.isArray(terrainObjects.previousWalls)
    ? terrainObjects.previousWalls
    : [];
  const swamps = Array.isArray(terrainObjects.previousSwamps)
    ? terrainObjects.previousSwamps
    : [];
  return [...walls, ...swamps];
}

module.exports = {
  collectDisplayObjects,
  collectDisplayTree,
  destroyTransientDisplayObject,
  resetAnimatedSpritePhase,
  resetRendererScene,
  retainedTerrainObjects,
};
