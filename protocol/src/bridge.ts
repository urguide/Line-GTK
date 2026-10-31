/** Thin LINE adapter. Rust owns all history persistence, scheduling and media placement. */
import type { Client } from "@evex/linejs";
import { TalkMessage } from "@evex/linejs";
import { join } from "@std/path";
import { coerceI64 } from "./messages.ts";
import { packRaw, unpackRaw } from "./raw_codec.ts";

type Json = Record<string, unknown>;
type Cursor = { messageId: string; deliveredTime: string };

export function createBridge(runtime: {
  client: () => Client | null;
  summarize: (raw: unknown) => Promise<Json>;
  transferDir: string;
}) {
  let busy = false;
  async function exclusive<T>(task: () => Promise<T>): Promise<T> {
    // A host timeout must not start a second LINE request while the original
    // operation is still completing. This is a guard, not another scheduler.
    if (busy) throw new Error("bridge_busy");
    busy = true;
    try {
      return await task();
    } finally {
      busy = false;
    }
  }
  async function fetchPage(chatMid: string, before?: Cursor | null) {
    const client = runtime.client();
    if (!client) throw new Error("not_logged_in");
    let end = before;
    if (!end) {
      const boxes = await client.base.talk.getMessageBoxes({
        messageBoxListRequest: {},
      });
      const last = boxes.messageBoxes.find((box) => box.id === chatMid)
        ?.lastDeliveredMessageId;
      if (last) {
        end = {
          messageId: String(last.messageId),
          deliveredTime: String(last.deliveredTime),
        };
      }
    }
    if (!end) return { messages: [], nextCursor: null, hasMore: false };
    const raw = await client.base.talk.getPreviousMessagesV2WithRequest({
      request: {
        messageBoxId: chatMid,
        endMessageId: {
          messageId: coerceI64(end.messageId),
          deliveredTime: coerceI64(end.deliveredTime),
        },
        messagesCount: 20,
      },
    });
    const messages = [];
    for (const message of raw) {
      if (String(message.id) === before?.messageId) continue;
      messages.push({
        ...await runtime.summarize(message),
        deliveredTime: String(message.deliveredTime),
        rawMessage: packRaw(message),
      });
    }
    const oldest = raw.at(-1);
    const nextCursor = oldest
      ? {
        messageId: String(oldest.id),
        deliveredTime: String(oldest.deliveredTime),
      }
      : null;
    return {
      messages,
      nextCursor,
      hasMore: raw.length === 20 && messages.length > 0 &&
        nextCursor?.messageId !== before?.messageId,
    };
  }

  async function download(message: Json) {
    const client = runtime.client();
    if (!client) throw new Error("not_logged_in");
    const id = String(message.id ?? "");
    if (!/^\d+$/.test(id)) throw new Error("invalid_message_id");
    let blob: Blob;
    if (message.contentType === "STICKER") {
      const sticker = String(message.stickerId ?? "");
      if (!/^\d+$/.test(sticker)) throw new Error("invalid_sticker_id");
      let response = await fetch(
        `https://stickershop.line-scdn.net/stickershop/v1/sticker/${sticker}/android/sticker_animation.png`,
      );
      if (!response.ok) {
        response = await fetch(
          `https://stickershop.line-scdn.net/stickershop/v1/sticker/${sticker}/android/sticker.png`,
        );
      }
      if (!response.ok) throw new Error(`sticker download: ${response.status}`);
      blob = await response.blob();
    } else {
      if (!message.rawMessage) {
        throw new Error("media metadata unavailable; refresh this page");
      }
      const raw = unpackRaw(message.rawMessage) as Parameters<
        typeof TalkMessage.fromRawTalk
      >[0];
      const tm = await TalkMessage.fromRawTalk(raw, client);
      blob = await tm.getData(false);
    }
    const mime = blob.type.toLowerCase();
    const extension = mime.includes("png")
      ? "png"
      : mime.includes("gif")
      ? "gif"
      : mime.includes("webp")
      ? "webp"
      : message.contentType === "IMAGE"
      ? "jpg"
      : message.contentType === "VIDEO"
      ? "mp4"
      : mime.includes("mpeg")
      ? "mp3"
      : mime.includes("ogg")
      ? "ogg"
      : message.contentType === "AUDIO"
      ? "m4a"
      : "bin";
    await Deno.mkdir(runtime.transferDir, { recursive: true, mode: 0o700 });
    const path = join(runtime.transferDir, `${id}-${crypto.randomUUID()}.part`);
    const file = await Deno.open(path, {
      createNew: true,
      write: true,
      mode: 0o600,
    });
    await blob.stream().pipeTo(file.writable);
    return { path, extension, bytes: blob.size };
  }
  return {
    fetchPage: (chat: string, before?: Cursor | null) =>
      exclusive(() => fetchPage(chat, before)),
    download: (message: Json) => exclusive(() => download(message)),
  };
}
