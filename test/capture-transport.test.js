"use strict";

const assert = require("node:assert/strict");
const { EventEmitter } = require("node:events");
const test = require("node:test");

const {
  chooseEncoderConfig,
  createEncoderGate,
  createFifoWriter,
} = require("../capture-transport");

test("createEncoderGate does not lose a dequeue that races waitForCapacity", async () => {
  const listeners = new Set();
  const encoder = {
    encodeQueueSize: 2,
    addEventListener(name, listener) {
      assert.equal(name, "dequeue");
      listeners.add(listener);
    },
    removeEventListener(name, listener) {
      listeners.delete(listener);
    },
  };
  const gate = createEncoderGate(encoder, () => null);

  // Start waiting while the queue is full, then fire dequeue before the microtask
  // continues — the waiter must observe capacity without hanging.
  const wait = gate.waitForCapacity(2);
  queueMicrotask(() => {
    encoder.encodeQueueSize = 0;
    for (const listener of listeners) listener();
  });
  await Promise.race([
    wait,
    new Promise((_, reject) => {
      setTimeout(() => reject(new Error("encoder gate hung on lost dequeue")), 50);
    }),
  ]);
  gate.destroy();
});

test("createFifoWriter waitWritable survives drain race", async () => {
  const stream = new EventEmitter();
  let writeCalls = 0;
  stream.write = () => {
    writeCalls++;
    // First write blocks; subsequent writes after drain succeed.
    return writeCalls > 1;
  };
  stream.end = (cb) => { cb(); };

  const writer = createFifoWriter(stream);
  writer.enqueue(Buffer.from("a"));
  assert.equal(writeCalls, 1);

  const wait = writer.waitWritable();
  queueMicrotask(() => stream.emit("drain"));
  await Promise.race([
    wait,
    new Promise((_, reject) => {
      setTimeout(() => reject(new Error("fifo writer hung on lost drain")), 50);
    }),
  ]);
  await writer.finish();
});

test("chooseEncoderConfig prefers UA-adjusted knobs but forces annexb/realtime geometry", async () => {
  const VideoEncoderClass = {
    async isConfigSupported(proposed) {
      return {
        supported: true,
        config: {
          ...proposed,
          bitrate: proposed.bitrate - 1,
          hardwareAcceleration: "prefer-software",
          width: 1,
          height: 1,
          latencyMode: "quality",
          avc: { format: "avc" },
        },
      };
    },
  };
  const config = await chooseEncoderConfig(VideoEncoderClass, {
    width: 64,
    height: 64,
    bitrate: 1_000_000,
    fps: 30,
  });
  assert.equal(config.bitrate, 999_999);
  assert.equal(config.hardwareAcceleration, "prefer-software");
  assert.equal(config.width, 64);
  assert.equal(config.height, 64);
  assert.equal(config.avc.format, "annexb");
  assert.equal(config.latencyMode, "realtime");
});

test("createFifoWriter.finish rejects when stream.end reports an error", async () => {
  const stream = new EventEmitter();
  stream.write = () => true;
  stream.end = (cb) => { cb(new Error("end failed")); };
  const writer = createFifoWriter(stream);
  await assert.rejects(() => writer.finish(), /end failed/);
});
