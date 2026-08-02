"use strict";

const assert = require("node:assert/strict");
const { EventEmitter } = require("node:events");
const test = require("node:test");

const {
  chooseEncoderConfig,
  createEncoderGate,
  createFifoFdWriter,
  createFifoWriter,
  openFifoForWrite,
} = require("../capture-transport");

test("openFifoForWrite returns its bounded nonblocking probe descriptor", async () => {
  const opens = [];
  const closes = [];
  const fileSystem = {
    constants: { O_WRONLY: 1, O_NONBLOCK: 2 },
    open(path, flags, callback) {
      opens.push([path, flags]);
      queueMicrotask(() => callback(null, 10));
    },
    close(fd, callback) {
      closes.push(fd);
      queueMicrotask(() => callback(null));
    },
  };

  const fd = await openFifoForWrite("/private/capture.fifo", 100, 1, fileSystem);
  assert.equal(fd, 10);
  assert.deepEqual(opens, [["/private/capture.fifo", 3]]);
  assert.deepEqual(closes, []);
});

test("createFifoFdWriter retries EAGAIN and completes partial writes", async () => {
  const writes = [];
  const closes = [];
  let call = 0;
  const fileSystem = {
    write(fd, buffer, offset, length, position, callback) {
      writes.push({ fd, buffer: buffer.toString(), offset, length, position });
      call++;
      queueMicrotask(() => {
        if (call === 1) callback(Object.assign(new Error("full"), { code: "EAGAIN" }));
        else callback(null, call === 2 ? 2 : length);
      });
    },
    close(fd, callback) {
      closes.push(fd);
      queueMicrotask(() => callback(null));
    },
  };
  const writer = createFifoFdWriter(12, fileSystem, 0);
  writer.enqueue(Buffer.from("abcd"));
  await writer.finish();
  assert.deepEqual(writes.map(({ offset, length }) => [offset, length]), [
    [0, 4],
    [0, 4],
    [2, 2],
  ]);
  assert.equal(writer.writtenBytes, 4);
  assert.equal(writer.pendingBytes, 0);
  assert.deepEqual(closes, [12]);
});

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
