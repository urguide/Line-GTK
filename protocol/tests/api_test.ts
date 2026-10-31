import { assertEquals, assertRejects } from "@std/assert";
import type { Client } from "@evex/linejs";
import { createApi } from "../src/api.ts";
import { packRaw, unpackRaw } from "../src/raw_codec.ts";
import { coerceI64 } from "../src/messages.ts";
import { unsendFromOperation } from "../src/events.ts";

Deno.test("plain OBS attachment RPC preserves destination, GIF, filename and duration without duplicate sends", async () => {
  const directory = await Deno.makeTempDir();
  try {
    await Deno.writeFile(`${directory}/upload.bin`, new Uint8Array([1, 2, 3]));
    const seen: unknown[][] = [];
    const client = {
      base: {
        obs: {
          uploadObjTalk: (...args: unknown[]) => {
            seen.push(args);
            return Promise.resolve({
              objId: "9007199254740993",
              objHash: "hash",
            });
          },
        },
        talk: {
          sendMessage: () => {
            throw new Error("must not duplicate OBS send");
          },
        },
        e2ee: {
          encryptByKeyMaterial: () => {
            throw new Error("not an E2EE destination");
          },
        },
      },
    } as unknown as Client;
    const api = createApi(() => client, directory);
    for (
      const [chatMid, oType] of [["r-room", "gif"], ["m-square", "video"], [
        "t-square",
        "audio",
      ]]
    ) {
      assertEquals(
        await api("bridge_upload_plain_media", {
          chatMid,
          oType,
          transferName: "upload.bin",
          fileName: "測試.bin",
          mime: "image/gif",
          durationMs: 3142,
        }),
        { objId: "9007199254740993", objHash: "hash" },
      );
    }
    assertEquals(
      seen.map((args) => [args[0], args[1], args[3], args[4], args[5]]),
      [["r-room", "gif", undefined, "測試.bin", 3142], [
        "m-square",
        "video",
        undefined,
        "測試.bin",
        3142,
      ], ["t-square", "audio", undefined, "測試.bin", 3142]],
    );
    assertEquals(
      [...new Uint8Array(await (seen[0][2] as Blob).arrayBuffer())],
      [1, 2, 3],
    );
    await assertRejects(
      () => api("bridge_upload_plain_media", { transferName: "../secret" }),
      Error,
      "invalid_transfer_name",
    );
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("API forwards exact read receipt, reply and withdrawal IDs", async () => {
  const seen: Record<string, unknown>[] = [];
  const client = {
    base: {
      getReqseq: () => Promise.resolve(4),
      talk: {
        sendMessage: (p: Record<string, unknown>) => {
          seen.push(p);
          return Promise.resolve({ id: "9007199254740994" });
        },
        sendChatChecked: (p: Record<string, unknown>) => {
          seen.push(p);
          return Promise.resolve();
        },
        unsendMessage: (p: Record<string, unknown>) => {
          seen.push(p);
          return Promise.resolve();
        },
      },
    },
  } as unknown as Client;
  const api = createApi(() => client, "/unused");
  await api("bridge_send_text", {
    chatMid: "u1",
    text: "reply",
    relatedMessageId: "9007199254740993",
    e2ee: true,
  });
  await api("bridge_mark_read", {
    chatMid: "u1",
    lastMessageId: "9007199254740993",
  });
  await api("bridge_unsend", { chatMid: "u1", messageId: "9007199254740993" });
  assertEquals(seen[0].relatedMessageId, "9007199254740993");
  assertEquals(seen[0].e2ee, true);
  assertEquals(seen[1], {
    chatMid: "u1",
    lastMessageId: "9007199254740993",
    seq: 4,
  });
  assertEquals(seen[2], { messageId: "9007199254740993", seq: 4 });
});
Deno.test("API rejects concurrent requests rather than introducing a second queue", async () => {
  const gate = Promise.withResolvers<unknown>();
  const client = {
    base: { talk: { getAllContactIds: () => gate.promise } },
  } as unknown as Client;
  const api = createApi(() => client, "/unused");
  const first = api("bridge_contact_ids", {});
  await assertRejects(
    () => api("bridge_contact_ids", {}),
    Error,
    "bridge_busy",
  );
  gate.resolve([]);
  await first;
});

Deno.test("contact fallbacks expose unmodified profile data to Rust", async () => {
  const profile = {
    targetProfileDetail: { profileName: "Friend", picturePath: "/avatar" },
  };
  const client = {
    fetchUsers: () => Promise.resolve([{ mid: "u1", raw: profile }]),
    base: {
      talk: {
        getContact: ({ mid }: { mid: string }) =>
          Promise.resolve({ mid, displayName: "Single" }),
      },
    },
  } as unknown as Client;
  const api = createApi(() => client, "/unused");
  assertEquals(await api("bridge_users", {}), [{ mid: "u1", raw: profile }]);
  assertEquals(await api("bridge_contact", { mid: "u2" }), {
    mid: "u2",
    displayName: "Single",
  });
});
Deno.test("media transfer rejects paths outside the exchange directory", async () => {
  const api = createApi(() => ({} as Client), "/unused");
  await assertRejects(
    () => api("bridge_send_media", { transferName: "../secret" }),
    Error,
    "invalid_transfer_name",
  );
});
Deno.test("wire codecs retain i64 and bytes and decode withdrawal events", () => {
  assertEquals(coerceI64("9007199254740993"), 9007199254740993n);
  const raw = unpackRaw(
    packRaw({ id: 9007199254740993n, chunks: [new Uint8Array([0, 255])] }),
  ) as { id: string; chunks: Uint8Array[] };
  assertEquals(raw.id, "9007199254740993");
  assertEquals([...raw.chunks[0]], [0, 255]);
  assertEquals(
    unsendFromOperation({
      type: "NOTIFIED_DESTROY_MESSAGE",
      param1: "group",
      param2: "9007199254740993",
    }),
    { chatMid: "group", messageId: "9007199254740993" },
  );
});
