import { Buffer } from "node:buffer";

/** Lossless IPC encoding for LINE i64 identifiers and E2EE byte buffers. */
export function packRaw(raw: unknown): unknown {
  return JSON.parse(JSON.stringify(raw, (_key, value) => {
    if (typeof value === "bigint") return value.toString();
    if (value instanceof Uint8Array) {
      return { type: "Buffer", data: [...value] };
    }
    return value;
  }));
}

export function unpackRaw(raw: unknown): unknown {
  return JSON.parse(JSON.stringify(raw), (_key, value) => {
    if (value?.type === "Buffer" && Array.isArray(value.data)) {
      return Buffer.from(value.data);
    }
    return value;
  });
}
