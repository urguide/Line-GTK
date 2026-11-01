import { assertEquals, assertRejects, assertThrows } from "@std/assert";
import type { Client, TalkMessage } from "@evex/linejs";
import { Protocols, Thrift } from "@evex/linejs/thrift";
import { Buffer } from "node:buffer";
import { createApi } from "../src/api.ts";
import { decryptVideo, downloadMedia } from "../src/media.ts";

const key = new Uint8Array(32).fill(42);
const keyMaterial = Buffer.from(key).toString("base64");

// Independent WebCrypto producer: exercise the LINE wire format, not a
// round-trip through the implementation's own signing/encryption functions.
async function fixture(size: number, legacy = false) {
  const plain = Uint8Array.from({ length: size }, (_, i) => i % 251);
  const hkdf = await crypto.subtle.importKey("raw", key, "HKDF", false, [
    "deriveBits",
  ]);
  const derived = new Uint8Array(
    await crypto.subtle.deriveBits(
      {
        name: "HKDF",
        hash: "SHA-256",
        salt: new Uint8Array(),
        info: new TextEncoder().encode("FileEncryption"),
      },
      hkdf,
      608,
    ),
  );
  const aes = await crypto.subtle.importKey(
    "raw",
    derived.slice(0, 32),
    "AES-CTR",
    false,
    ["encrypt"],
  );
  const counter = new Uint8Array(16);
  counter.set(derived.slice(64));
  const ciphertext = new Uint8Array(
    await crypto.subtle.encrypt(
      { name: "AES-CTR", counter, length: 32 },
      aes,
      plain,
    ),
  );
  const hashes = new Uint8Array(Math.ceil(size / 131072) * 32);
  for (let i = 0; i < hashes.length / 32; i++) {
    hashes.set(
      new Uint8Array(
        await crypto.subtle.digest(
          "SHA-256",
          ciphertext.slice(i * 131072, (i + 1) * 131072),
        ),
      ),
      i * 32,
    );
  }
  const hmac = await crypto.subtle.importKey(
    "raw",
    derived.slice(32, 64),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const tag = new Uint8Array(
    await crypto.subtle.sign("HMAC", hmac, legacy ? ciphertext : hashes),
  );
  const encrypted = new Uint8Array(size + 32);
  encrypted.set(ciphertext);
  encrypted.set(tag, size);
  return { plain, encrypted };
}

Deno.test("video chunk MAC supports boundaries, partial chunks and legacy linejs originals", async () => {
  for (const size of [0, 17, 131071, 131072, 131073, 262144, 300000]) {
    const { plain, encrypted } = await fixture(size);
    assertEquals(decryptVideo(encrypted, keyMaterial), plain);
  }
  const { plain, encrypted } = await fixture(300000, true);
  assertEquals(decryptVideo(encrypted, keyMaterial), plain);
});

Deno.test("video rejects modified chunks, tags, truncation and wrong keys in both MAC formats", async () => {
  for (const legacy of [false, true]) {
    const { encrypted } = await fixture(300000, legacy);
    for (const index of [0, 131072, 299999, encrypted.length - 1]) {
      const damaged = encrypted.slice();
      damaged[index] ^= 1;
      assertThrows(
        () => decryptVideo(damaged, keyMaterial),
        Error,
        "HMAC verification failed",
      );
    }
    assertThrows(
      () => decryptVideo(encrypted.slice(0, -1), keyMaterial),
      Error,
      "HMAC verification failed",
    );
    assertThrows(
      () => decryptVideo(encrypted, Buffer.alloc(32, 7).toString("base64")),
      Error,
      "HMAC verification failed",
    );
  }
  assertThrows(
    () => decryptVideo(new Uint8Array(31), keyMaterial),
    Error,
    "too short",
  );
  assertThrows(
    () => decryptVideo(new Uint8Array(32), "bad"),
    Error,
    "invalid_video_key_material",
  );
});

Deno.test("disk and memory RPCs decrypt video with authenticated OBS metadata and lossless IDs", async () => {
  const { plain, encrypted } = await fixture(131073);
  const directory = await Deno.makeTempDir();
  try {
    let fail = false;
    let calls = 0;
    const raw = {
      id: "9007199254740993",
      to: "c-group",
      contentType: "VIDEO",
      contentMetadata: { SID: "emv", OID: "video-object" },
      chunks: [Buffer.from([1])],
    };
    const client = {
      base: {
        e2ee: {
          decryptE2EEDataMessage: () =>
            Promise.resolve({ keyMaterial, fileName: "clip.mp4" }),
        },
        obs: {
          downloadObjectForService: (
            options: {
              obsPath: string;
              oid: string;
              addHeaders: Record<string, string>;
            },
          ) => {
            calls++;
            assertEquals(options.obsPath, "talk/emv");
            assertEquals(options.oid, "video-object");
            const meta = JSON.parse(
              Buffer.from(options.addHeaders["X-Talk-Meta"], "base64")
                .toString(),
            );
            const parsed = new Thrift().readThriftStruct(
              Buffer.from(meta.message, "base64"),
              Protocols[3],
            );
            assertEquals(parsed, { 4: raw.id, 27: [] });
            if (fail) throw new Error("Object download failed: HTTP 404");
            return Promise.resolve(new Blob([encrypted]));
          },
        },
      },
    } as unknown as Client;
    const api = createApi(() => client, directory, () =>
      Promise.resolve({
        raw,
        getData: () => {
          throw new Error("generic file HMAC must not handle video");
        },
      } as unknown as TalkMessage));
    const params = { message: { rawMessage: raw } };
    const disk = await api("bridge_download", params) as {
      path: string;
      bytes: number;
    };
    assertEquals(disk.bytes, plain.length);
    assertEquals(await Deno.readFile(disk.path), plain);
    // Numeric content types occur in outgoing messages too.
    Object.assign(raw, { contentType: 2 });
    const memory = await api("bridge_memory_download", params) as {
      token: string;
      bytes: number;
    };
    assertEquals(memory.bytes, plain.length);
    const chunk = await api("bridge_memory_chunk", {
      token: memory.token,
      offset: 0,
    }) as { data: string };
    assertEquals(new Uint8Array(Buffer.from(chunk.data, "base64")), plain);
    fail = true;
    await assertRejects(
      () => api("bridge_download", params),
      Error,
      "HTTP 404",
    );
    fail = false;
    encrypted[0] ^= 1;
    for (const method of ["bridge_download", "bridge_memory_download"]) {
      await assertRejects(
        () => api(method, params),
        Error,
        "HMAC verification failed",
      );
    }
    assertEquals(calls, 5);
    const files = [];
    for await (const entry of Deno.readDir(directory)) files.push(entry.name);
    assertEquals(files.length, 1); // Only the first, authenticated download was written.
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("non-video, plaintext video and URL media retain the library download path", async () => {
  for (
    const raw of [
      { contentType: "IMAGE", chunks: [1] },
      { contentType: "AUDIO", chunks: [1] },
      { contentType: "FILE", chunks: [1] },
      { contentType: "VIDEO", chunks: [] },
      { contentType: "VIDEO" },
      {
        contentType: "VIDEO",
        chunks: [1],
        contentMetadata: { DOWNLOAD_URL: "https://example.invalid/video" },
      },
    ]
  ) {
    const result = await downloadMedia({} as Client, {
      raw,
      getData: (preview: boolean) => {
        assertEquals(preview, false);
        return Promise.resolve(new Blob(["library bytes"]));
      },
    } as unknown as TalkMessage);
    assertEquals(await result.text(), "library bytes");
  }
});
