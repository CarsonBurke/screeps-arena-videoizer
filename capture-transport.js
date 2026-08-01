"use strict";

const fs = require("fs");

/**
 * WebCodecs encoder / Annex-B FIFO transport helpers.
 * Orthogonal to renderer semantics; kept separate so Node-only tests can exercise
 * backpressure without Pixi.
 */

function openFifoForWrite(fifoPath, timeoutMs, retryMs = 10) {
  const flags = fs.constants.O_WRONLY | fs.constants.O_NONBLOCK;
  return new Promise((resolve, reject) => {
    const deadline = Date.now() + timeoutMs;
    let retryTimer = null;
    let settled = false;

    const finish = (error, fd) => {
      if (settled) {
        if (typeof fd === "number") fs.close(fd, () => {});
        return;
      }
      settled = true;
      if (retryTimer) clearTimeout(retryTimer);
      if (error) reject(error);
      else resolve(fd);
    };

    const attempt = () => {
      fs.open(fifoPath, flags, (error, fd) => {
        if (!error) {
          finish(null, fd);
          return;
        }
        // ENXIO is expected until a reader has opened the FIFO.
        if (error.code !== "ENXIO") {
          finish(error);
          return;
        }
        const remainingMs = deadline - Date.now();
        if (remainingMs <= 0) {
          finish(new Error(`board capture: timed out waiting for FIFO reader at ${fifoPath}`));
          return;
        }
        retryTimer = setTimeout(attempt, Math.min(retryMs, remainingMs));
      });
    };

    attempt();
  });
}

function createEncoderGate(encoder, getError) {
  const waiters = new Set();
  const wake = () => {
    for (const resolve of waiters) resolve();
    waiters.clear();
  };

  if (typeof encoder.addEventListener === "function") {
    encoder.addEventListener("dequeue", wake);
  } else if ("ondequeue" in encoder) {
    encoder.ondequeue = wake;
  } else {
    throw new Error("board capture: this WebCodecs encoder has no dequeue event support");
  }

  return {
    wake,
    async waitForCapacity(limit) {
      // Register the waiter before re-checking encodeQueueSize so a dequeue that
      // fires between the check and the park cannot be lost.
      while (true) {
        const error = getError();
        if (error) throw error;
        if (encoder.encodeQueueSize < limit) return;
        await new Promise((resolve) => {
          waiters.add(resolve);
          if (encoder.encodeQueueSize < limit || getError()) resolve();
        });
      }
    },
    destroy() {
      if (typeof encoder.removeEventListener === "function") {
        encoder.removeEventListener("dequeue", wake);
      } else if (encoder.ondequeue === wake) {
        encoder.ondequeue = null;
      }
      wake();
    },
  };
}

function createFifoWriter(stream) {
  const queue = [];
  const waiters = new Set();
  let blocked = false;
  let ended = false;
  let error = null;
  let pendingBytes = 0;
  let writtenBytes = 0;

  const wake = () => {
    for (const resolve of waiters) resolve();
    waiters.clear();
  };

  const pump = () => {
    if (blocked || ended || error) return;
    while (queue.length > 0) {
      const buffer = queue.shift();
      pendingBytes -= buffer.length;
      writtenBytes += buffer.length;
      if (!stream.write(buffer)) {
        blocked = true;
        stream.once("drain", () => {
          blocked = false;
          pump();
          wake();
        });
        break;
      }
    }
    wake();
  };

  stream.on("error", (streamError) => {
    error = error || streamError;
    wake();
  });

  return {
    enqueue(buffer) {
      if (ended) throw new Error("board capture: attempted to write after FIFO end");
      if (error) throw error;
      queue.push(buffer);
      pendingBytes += buffer.length;
      pump();
    },
    async waitWritable() {
      // Subscribe before re-checking blocked/queue so a drain between the
      // condition and the park cannot leave the producer hung forever.
      while (true) {
        if (error) throw error;
        if (!blocked && queue.length === 0) return;
        await new Promise((resolve) => {
          waiters.add(resolve);
          if ((!blocked && queue.length === 0) || error) resolve();
        });
      }
    },
    async finish() {
      await this.waitWritable();
      ended = true;
      await new Promise((resolve, reject) => {
        const onError = (streamError) => {
          error = error || streamError;
          reject(streamError);
        };
        stream.once("error", onError);
        stream.end((endError) => {
          stream.removeListener("error", onError);
          if (endError) {
            error = error || endError;
            reject(endError);
            return;
          }
          resolve();
        });
      });
      if (error) throw error;
    },
    destroy() {
      ended = true;
      wake();
    },
    get error() { return error; },
    get pendingBytes() { return pendingBytes; },
    get writtenBytes() { return writtenBytes; },
  };
}

async function chooseEncoderConfig(VideoEncoderClass, config) {
  const candidates = [
    { codec: "avc1.640033", hardwareAcceleration: "prefer-hardware" },
    { codec: "avc1.640033", hardwareAcceleration: "no-preference" },
    { codec: "avc1.42E033", hardwareAcceleration: "no-preference" },
  ];
  for (const candidate of candidates) {
    const proposed = {
      codec: candidate.codec,
      width: config.width,
      height: config.height,
      bitrate: config.bitrate,
      framerate: config.fps,
      // Annex-B is handed to ffmpeg as a raw stream, so preserve display order.
      latencyMode: "realtime",
      avc: { format: "annexb" },
      hardwareAcceleration: candidate.hardwareAcceleration,
    };
    try {
      const support = await VideoEncoderClass.isConfigSupported(proposed);
      if (support && support.supported) {
        // Prefer UA-adjusted knobs (bitrate/codec/hw) but force capture invariants
        // the remux path depends on. Never let the UA switch to AVCC/quality
        // reorder or resize away from the canvas geometry already locked.
        const accepted = { ...proposed, ...(support.config || {}) };
        accepted.width = proposed.width;
        accepted.height = proposed.height;
        accepted.latencyMode = "realtime";
        accepted.avc = { ...(accepted.avc || {}), format: "annexb" };
        return accepted;
      }
    } catch (_) {
      // Try the next profile/backend.
    }
  }
  throw new Error(
    `board capture: no supported quality H.264 encoder for ${config.width}x${config.height}`,
  );
}

/**
 * Resolve transport paths for a capture run.
 *
 * Security policy:
 * - Prefer explicit option paths (tests / host integration).
 * - Otherwise require a valid capture-id and a private cache root under HOME
 *   (or options.transportDir).
 * - Never fall back to fixed shared /tmp names (symlink clobber risk).
 * - Env path overrides are accepted only when they resolve under the private
 *   transport directory for this capture-id.
 */
function resolveCaptureTransport(options = {}, params = new URLSearchParams(), env = {}) {
  const rawCaptureId = params.get("capture-id");
  const captureId = rawCaptureId && /^capture-[0-9]+$/.test(rawCaptureId)
    ? rawCaptureId
    : null;
  const transportDir = options.transportDir
    || (captureId && env.HOME ? `${env.HOME}/.cache/screeps-arena-videoizer` : null);
  const transportPrefix = transportDir && captureId
    ? `${transportDir}/${captureId}`
    : null;

  const underTransportRoot = (candidate) => {
    if (!candidate || !transportDir) return false;
    const resolvedRoot = pathResolve(transportDir);
    const resolvedCandidate = pathResolve(candidate);
    return resolvedCandidate === resolvedRoot
      || resolvedCandidate.startsWith(`${resolvedRoot}/`);
  };

  const pickPath = (optionValue, envName, suffix) => {
    if (optionValue) return String(optionValue);
    if (transportPrefix) {
      const preferred = `${transportPrefix}${suffix}`;
      const envValue = env[envName];
      if (envValue && underTransportRoot(envValue)) return String(envValue);
      return preferred;
    }
    // Without a capture-id prefix, only explicit options may select paths.
    // Env overrides alone are ignored to avoid arbitrary writes from a poisoned
    // Steam process environment.
    return null;
  };

  return Object.freeze({
    rawCaptureId: rawCaptureId || null,
    captureId,
    transportDir,
    transportPrefix,
    fifoPath: pickPath(options.fifoPath, "SCREEPS_ARENA_BOARD_CAPTURE_FIFO", ".fifo"),
    errorFile: pickPath(options.errorFile, "SCREEPS_ARENA_BOARD_CAPTURE_ERROR", ".error"),
    doneFile: pickPath(options.doneFile, "SCREEPS_ARENA_BOARD_CAPTURE_DONE", ".done"),
    debugFile: pickPath(options.debugFile, "SCREEPS_ARENA_BOARD_CAPTURE_DEBUG", ".debug.log"),
    metaFile: pickPath(options.metaFile, "SCREEPS_ARENA_BOARD_CAPTURE_META", ".meta"),
    telemetryFile: pickPath(
      options.telemetryFile,
      "SCREEPS_ARENA_BOARD_CAPTURE_TELEMETRY",
      ".telemetry.json",
    ),
  });
}

function pathResolve(value) {
  // Local helper so this module does not pull path just for security checks in
  // environments that only need the FIFO writer (still works under Node).
  const path = require("path");
  return path.resolve(String(value));
}

module.exports = {
  chooseEncoderConfig,
  createEncoderGate,
  createFifoWriter,
  openFifoForWrite,
  resolveCaptureTransport,
};
