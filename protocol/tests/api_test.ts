import { assertEquals, assertRejects } from "@std/assert";
import type { Client, TalkMessage } from "@evex/linejs";
import { createApi } from "../src/api.ts";
import { packRaw, unpackRaw, wireLine } from "../src/raw_codec.ts";
import { coerceI64 } from "../src/messages.ts";
import { unsendFromOperation } from "../src/events.ts";

Deno.test("mention members request only current chat participants and metadata survives send", async () => {
  const metadata = { MENTION: '{"MENTIONEES":[{"S":"3","E":"10","M":"u1"}]}' };
  const client = {
    base: {
      talk: {
        getChat: (p: unknown) => {
          assertEquals(p, {
            chatMid: "c1",
            withMembers: true,
            withInvitees: false,
          });
          return Promise.resolve({
            extra: { groupExtra: { memberMids: { u1: 1 } } },
          });
        },
        sendMessage: (p: Record<string, unknown>) => {
          assertEquals(p.contentMetadata, metadata);
          assertEquals(p.text, "😀 @Johnny");
          assertEquals(p.relatedMessageId, "123");
          return Promise.resolve({ id: "124" });
        },
      },
    },
  } as unknown as Client;
  const api = createApi(() => client, "/unused");
  assertEquals(await api("bridge_chat_members", { chatMid: "c1" }), {
    extra: { groupExtra: { memberMids: { u1: 1 } } },
  });
  const sent = await api("bridge_send_text", {
    chatMid: "c1",
    text: "😀 @Johnny",
    contentMetadata: metadata,
    relatedMessageId: "123",
  }) as { rawMessage: { contentMetadata: unknown } };
  assertEquals(sent.rawMessage.contentMetadata, metadata);
});

Deno.test("inline emoji metadata accompanies encrypted text and the saved outgoing message", async () => {
  const metadata = {
    REPLACE: JSON.stringify({
      sticon: {
        resources: [{
          S: 3,
          E: 6,
          productId: "abc",
          sticonId: "001",
          version: 2,
          resourceType: "STATIC",
        }],
      },
    }),
    STICON_OWNERSHIP: '["abc"]',
  };
  const client = {
    base: {
      talk: {
        sendMessage: (p: Record<string, unknown>) => {
          assertEquals(p.text, "好😀(笑)");
          assertEquals(p.contentMetadata, metadata);
          assertEquals(p.e2ee, true);
          return Promise.resolve({ id: "123" });
        },
      },
    },
  } as unknown as Client;
  const result = await createApi(() => client, "/unused")("bridge_send_text", {
    chatMid: "u1",
    text: "好😀(笑)",
    contentMetadata: metadata,
  });
  assertEquals(
    (result as { rawMessage: { contentMetadata: unknown } }).rawMessage
      .contentMetadata,
    metadata,
  );
});

Deno.test("memory media transfers bytes without creating disk transfers", async () => {
  const bytes = new Uint8Array(300000).fill(17);
  const api = createApi(
    () => ({} as Client),
    "/nonexistent/no-disk-writes",
    () =>
      Promise.resolve({
        raw: {},
        getData: () => Promise.resolve(new Blob([bytes])),
      } as unknown as TalkMessage),
  );
  const start = await api("bridge_memory_download", {
    message: { rawMessage: {} },
  }) as { token: string; bytes: number };
  assertEquals(start.bytes, bytes.length);
  const first = await api("bridge_memory_chunk", {
    token: start.token,
    offset: 0,
  }) as { data: string };
  assertEquals(atob(first.data).length, 256 * 1024);
  const last = await api("bridge_memory_chunk", {
    token: start.token,
    offset: 256 * 1024,
  }) as { data: string };
  assertEquals(atob(last.data).length, bytes.length - 256 * 1024);
  await api("bridge_memory_release", { token: start.token });
  await assertRejects(
    () => api("bridge_memory_chunk", { token: start.token, offset: 0 }),
    Error,
    "invalid_memory_transfer",
  );
});

Deno.test("memory uploads reach OBS without staging files", async () => {
  const client = {
    base: {
      obs: {
        uploadObjTalk: async (
          chat: string,
          type: string,
          data: Blob,
        ) => {
          assertEquals(chat, "r1");
          assertEquals(type, "image");
          assertEquals(await data.text(), "image bytes");
          return { objId: "1", objHash: "hash" };
        },
      },
    },
  } as unknown as Client;
  const api = createApi(() => client, "/nonexistent/no-disk-writes");
  const { token } = await api("bridge_memory_upload_start", {}) as {
    token: string;
  };
  await api("bridge_memory_upload_chunk", { token, data: btoa("image bytes") });
  const params = {
    memoryToken: token,
    chatMid: "r1",
    oType: "image",
    fileName: "photo.png",
  };
  await api("bridge_upload_plain_media", params);
  await api("bridge_memory_upload_release", { token });
  await assertRejects(
    () => api("bridge_upload_plain_media", params),
    Error,
    "invalid_memory_upload",
  );
});

Deno.test("session and profile wire envelopes preserve bigint without crashing launcher", () => {
  const profile = {
    mid: "u1",
    displayName: "User",
    timestamp: 9007199254740993n,
    nested: { revision: 9007199254740994n },
  };
  for (
    const message of [{ event: "core_session", ...profile }, {
      event: "session",
      ...profile,
    }, { id: 1, ok: true, result: profile }]
  ) {
    const line = wireLine(message);
    assertEquals(line.endsWith("\n"), true);
    assertEquals(JSON.parse(line), packRaw(message));
  }
});

Deno.test("one decode failure preserves the page, original metadata and next cursor", async () => {
  const base = 9007199254740993n;
  const seen: string[] = [];
  let page = 0;
  const client = {
    base: {
      talk: {
        getPreviousMessagesV2WithRequest: (
          { request }: {
            request: {
              endMessageId: { messageId: bigint | number };
              messagesCount: number;
            };
          },
        ) => {
          assertEquals(request.messagesCount, 20);
          assertEquals(
            request.endMessageId.messageId,
            page === 0 ? base + 20n : base,
          );
          const raw = page++ === 0
            ? Array.from(
              { length: 20 },
              (_, i) => ({
                id: String(base + 19n - BigInt(i)),
                createdTime: 20 - i,
                deliveredTime: 20 - i,
                contentType: "NONE",
                contentMetadata: { e2eeVersion: "2" },
                chunks: [new Uint8Array([i])],
              }),
            )
            : [{
              id: String(base - 1n),
              createdTime: 0,
              deliveredTime: 0,
              contentType: "NONE",
              contentMetadata: {},
              chunks: [],
            }];
          return Promise.resolve(raw);
        },
      },
    },
  } as unknown as Client;
  const api = createApi(() => client, "/unused", (raw) => {
    seen.push(String(raw.id));
    if (String(raw.id) === String(base)) {
      raw.contentMetadata = { mutated: "yes" };
      throw new Error("sensitive decoder details must not leak");
    }
    return Promise.resolve(
      { raw: { ...raw, text: "decoded" } } as unknown as TalkMessage,
    );
  });
  const first = await api("bridge_fetch_page", {
    chatMid: "u1",
    before: { messageId: String(base + 20n), deliveredTime: "21" },
  }) as {
    messages: Record<string, unknown>[];
    nextCursor: { messageId: string; deliveredTime: string };
    hasMore: boolean;
  };
  assertEquals(first.messages.length, 20);
  assertEquals(seen.length, 20);
  assertEquals(first.hasMore, true);
  assertEquals(first.nextCursor, {
    messageId: String(base),
    deliveredTime: "1",
  });
  assertEquals(first.messages[19].decodeError, "message_decode_failed");
  assertEquals(first.messages[19].decodedMessage, null);
  const saved = first.messages[19].rawMessage as Record<string, unknown>;
  assertEquals(saved.contentMetadata, { e2eeVersion: "2" });
  assertEquals(JSON.stringify(first).includes("sensitive"), false);
  assertEquals(
    (first.messages[0].decodedMessage as Record<string, unknown>).text,
    "decoded",
  );
  const next = await api("bridge_fetch_page", {
    chatMid: "u1",
    before: first.nextCursor,
  }) as { messages: unknown[]; hasMore: boolean };
  assertEquals(next.messages.length, 1);
  assertEquals(next.hasMore, false);
});

Deno.test("sticker RPC requires Rust-selected version and propagates ownership errors", async () => {
  let count = 0;
  let reject = false;
  const client = {
    base: {
      talk: {
        sendMessage: (p: Record<string, unknown>) => {
          count++;
          assertEquals(
            (p.contentMetadata as Record<string, string>).STKVER,
            "8",
          );
          if (reject) throw new Error("USER_NOT_STICKER_OWNER");
          return Promise.resolve({ id: "123" });
        },
      },
    },
  } as unknown as Client;
  const api = createApi(() => client, "/unused");
  const p = { chatMid: "u1", stickerId: "10", packageId: "20", version: "8" };
  assertEquals(await api("bridge_send_sticker", p), {
    rawMessage: { id: "123" },
  });
  reject = true;
  await assertRejects(
    () => api("bridge_send_sticker", p),
    Error,
    "USER_NOT_STICKER_OWNER",
  );
  await assertRejects(
    () => api("bridge_send_sticker", { ...p, version: undefined }),
    Error,
    "sticker_version_required",
  );
  assertEquals(count, 2);
});

Deno.test("Rust-normalized GIF keeps animation flags and MIME in the E2EE RPC", async () => {
  const directory = await Deno.makeTempDir();
  try {
    await Deno.writeFile(
      `${directory}/animation.gif`,
      new Uint8Array([71, 73, 70]),
    );
    const uploads: Record<string, unknown>[] = [];
    let sent: Record<string, unknown> = {};
    const client = {
      base: {
        e2ee: {
          encryptByKeyMaterial: () =>
            Promise.resolve({
              keyMaterial: "a2V5",
              encryptedData: new Uint8Array([1, 2, 3]),
            }),
          encryptE2EEMessage: (_to: unknown, _data: unknown, type: unknown) => {
            assertEquals(type, 1);
            return Promise.resolve([new Uint8Array([4])]);
          },
        },
        obs: {
          uploadObjectForService: (options: Record<string, unknown>) => {
            uploads.push(options);
            return Promise.resolve({ objId: "123" });
          },
        },
        talk: {
          sendMessage: (options: Record<string, unknown>) => {
            sent = options;
            return Promise.resolve({ id: "123" });
          },
        },
      },
    } as unknown as Client;
    const api = createApi(() => client, directory);
    await api("bridge_send_media", {
      chatMid: "u1",
      transferName: "animation.gif",
      fileName: "Animation.GIF",
      extension: "gif",
      mime: "image/gif",
      oType: "gif",
    });
    assertEquals(uploads.length, 2);
    assertEquals(uploads[0].params, { type: "file", cat: "original" });
    assertEquals(sent.contentType, 1);
    const meta = sent.contentMetadata as Record<string, string>;
    assertEquals(JSON.parse(meta.MEDIA_CONTENT_INFO).animated, true);
    assertEquals(JSON.parse(meta.MEDIA_CONTENT_INFO).extension, "gif");
    assertEquals(meta.e2eeVersion, "2");
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("library E2EE RPC forwards original preview duration and errors without plaintext fallback", async () => {
  const directory = await Deno.makeTempDir();
  try {
    await Deno.writeFile(
      `${directory}/original.mp4`,
      new Uint8Array([1, 2, 3]),
    );
    await Deno.writeFile(`${directory}/preview.jpg`, new Uint8Array([4, 5]));
    let calls = 0;
    let reject = false;
    const client = {
      base: {
        obs: {
          uploadMediaByE2EE: async (
            options: {
              to: string;
              oType: string;
              data: Blob;
              preview?: Blob;
              filename: string;
              durationMs: number;
            },
          ) => {
            calls++;
            assertEquals(options.to, "c-group");
            assertEquals(options.oType, "video");
            assertEquals(options.filename, "影片.mp4");
            assertEquals(options.durationMs, 3200);
            assertEquals(
              [...new Uint8Array(await options.data.arrayBuffer())],
              [1, 2, 3],
            );
            assertEquals([
              ...new Uint8Array(await options.preview!.arrayBuffer()),
            ], [4, 5]);
            if (reject) {
              throw new Error("encrypted fallback rejected");
            }
            return { id: "9007199254740993" };
          },
          uploadObjTalk: () => {
            throw new Error("must not use plaintext");
          },
        },
      },
    } as unknown as Client;
    const api = createApi(() => client, directory);
    const p = {
      chatMid: "c-group",
      oType: "video",
      transferName: "original.mp4",
      previewName: "preview.jpg",
      fileName: "影片.mp4",
      durationMs: 3200,
    };
    assertEquals(await api("bridge_send_media_e2ee_fallback", p), {
      rawMessage: { id: "9007199254740993" },
    });
    reject = true;
    await assertRejects(
      () => api("bridge_send_media_e2ee_fallback", p),
      Error,
      "encrypted fallback rejected",
    );
    assertEquals(calls, 2);
    await assertRejects(
      () => api("bridge_send_media_e2ee_fallback", { ...p, chatMid: "r-room" }),
      Error,
      "e2ee_media_mid_unsupported",
    );
    assertEquals(calls, 2);
    await assertRejects(
      () =>
        api("bridge_send_media_e2ee_fallback", {
          ...p,
          transferName: "../secret",
        }),
      Error,
      "invalid_transfer_name",
    );
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});

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
