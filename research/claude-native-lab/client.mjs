import net from "node:net";
import { EventEmitter } from "node:events";
import { createWriteStream, openSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { randomUUID } from "node:crypto";

export class NativeClient extends EventEmitter {
  constructor(socketPath, recordPath) {
    super();
    this.pending = new Map();
    this.nextId = 1;
    this.events = [];
    this.failure = undefined;
    this.record = recordPath
      ? createWriteStream(recordPath, { fd: openSync(recordPath, "wx", 0o600), autoClose: true })
      : undefined;
    this.record?.on("error", (error) => {
      console.error(`Native capture failed: ${error.message}`);
      this.fail(error);
      this.socket.destroy();
    });
    this.socket = net.connect(socketPath);
    this.socket.setEncoding("utf8");
    let buffer = "";
    this.socket.on("data", (chunk) => {
      buffer += chunk;
      if (buffer.length > 16 * 1024 * 1024) {
        this.socket.destroy(new Error("Native response exceeds prototype's 16 MiB limit"));
        return;
      }
      let newline;
      while ((newline = buffer.indexOf("\n")) >= 0) {
        const line = buffer.slice(0, newline);
        buffer = buffer.slice(newline + 1);
        let message;
        try {
          message = JSON.parse(line);
          if (!message || typeof message !== "object" || Array.isArray(message))
            throw new Error("Expected a native protocol object");
        } catch (error) {
          this.socket.destroy(new Error(`Malformed native response: ${error.message}`));
          return;
        }
        this.record?.write(JSON.stringify({ at: Date.now(), direction: "incoming", message }) + "\n");
        if (message.event) {
          this.events.push(message);
          if (this.events.length > 1024) this.events.shift();
          this.emit("event", message);
        } else {
          const request = this.pending.get(message.id);
          if (request) {
            this.pending.delete(message.id);
            clearTimeout(request.timer);
            if (message.error) request.reject(new Error(message.error));
            else request.resolve(message.result);
          }
        }
      }
    });
    this.socket.on("error", (error) => this.fail(error));
    this.socket.on("close", () => {
      this.fail(new Error("Disconnected"));
      this.record?.end();
    });
  }

  fail(error) {
    if (this.failure) return;
    this.failure = error;
    for (const request of this.pending.values()) {
      clearTimeout(request.timer);
      request.reject(error);
    }
    this.pending.clear();
    this.emit("connection_error", error);
  }

  request(method, parameters = {}, timeout = 15000) {
    if (this.failure) return Promise.reject(this.failure);
    const id = this.nextId++;
    const message = { ...(method === "prompt" ? { submissionId: randomUUID() } : {}), ...parameters, id, method };
    this.record?.write(JSON.stringify({ at: Date.now(), direction: "outgoing", message }) + "\n");
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`Timed out: ${method}`));
      }, timeout);
      this.pending.set(id, { resolve, reject, timer });
      this.socket.write(JSON.stringify(message) + "\n");
    });
  }

  waitFor(predicate, timeout = 45000) {
    const existing = this.events.find(predicate);
    if (existing) return Promise.resolve(existing);
    if (this.failure) return Promise.reject(this.failure);
    return new Promise((resolve, reject) => {
      const cleanup = () => {
        clearTimeout(timer);
        this.off("event", listener);
        this.off("connection_error", disconnected);
      };
      const disconnected = (error) => {
        cleanup();
        reject(error);
      };
      const listener = (event) => {
        try {
          if (!predicate(event)) return;
          cleanup();
          resolve(event);
        } catch (error) {
          cleanup();
          reject(error);
        }
      };
      const timer = setTimeout(() => {
        cleanup();
        reject(new Error("Timed out waiting for native event"));
      }, timeout);
      this.on("event", listener);
      this.on("connection_error", disconnected);
    });
  }

  close() {
    this.fail(new Error("Disconnected by client"));
    this.socket.end();
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [socketPath, method = "snapshot", parameters = "{}"] = process.argv.slice(2);
  const client = new NativeClient(socketPath);
  try {
    console.log(JSON.stringify(await client.request(method, JSON.parse(parameters)), null, 2));
  } finally {
    client.close();
  }
}
