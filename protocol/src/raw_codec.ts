import { Buffer } from "node:buffer";

/** stdout NDJSON must tolerate LINE's 64-bit integer fields. */
export function wireLine(value: unknown): string {
  return JSON.stringify(
    value,
    (_key, item) => typeof item === "bigint" ? item.toString() : item,
  ) + "\n";
}

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
