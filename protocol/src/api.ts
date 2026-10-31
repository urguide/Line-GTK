/** LINE operations only: no UI caches, retry loops, media conversion or history policy. */
import { Client, TalkMessage } from "@evex/linejs";
import { coerceI64 } from "./messages.ts";
import { packRaw, unpackRaw } from "./raw_codec.ts";
import { Buffer } from "node:buffer";

type Json = Record<string, unknown>;
export function createApi(getClient: () => Client | null, transfer: string) {
  let busy = false;
  function client(): Client {
    const value = getClient();
    if (!value) throw new Error("not_logged_in");
    return value;
  }
  function transferPath(name: unknown): string {
    if (
      typeof name !== "string" || !/^[a-zA-Z0-9_.-]+$/.test(name) ||
      name.startsWith(".")
    ) throw new Error("invalid_transfer_name");
    return `${transfer}/${name}`;
  }
  async function rpc(method: string, p: Json): Promise<unknown> {
    const c = client();
    switch (method) {
      case "bridge_fetch_page": {
        let end = p.before as
          | { messageId: string; deliveredTime: string }
          | null;
        if (!end) {
          const boxes = await c.base.talk.getMessageBoxes({
            messageBoxListRequest: {},
          });
          const last = boxes.messageBoxes.find((box) => box.id === p.chatMid)
            ?.lastDeliveredMessageId;
          if (last) {
            end = {
              messageId: String(last.messageId),
              deliveredTime: String(last.deliveredTime),
            };
          }
        }
        if (!end) return { messages: [], nextCursor: null, hasMore: false };
        const raw = await c.base.talk.getPreviousMessagesV2WithRequest({
          request: {
            messageBoxId: String(p.chatMid),
            endMessageId: {
              messageId: coerceI64(end.messageId),
              deliveredTime: coerceI64(end.deliveredTime),
            },
            messagesCount: 20,
          },
        });
        const messages = [];
        for (const message of raw) {
          if (p.before && message.id === end.messageId) continue;
          const tm = await TalkMessage.fromRawTalk(message, c);
          messages.push({
            decodedMessage: packRaw(tm.raw),
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
            nextCursor?.messageId !== (p.before ? end.messageId : ""),
        };
      }
      case "bridge_boxes":
        return await c.base.talk.getMessageBoxes({ messageBoxListRequest: {} });
      case "bridge_contact_ids":
        return await c.base.talk.getAllContactIds({});
      case "bridge_contacts":
        return await c.base.talk.getContacts({ mids: p.mids as string[] });
      case "bridge_groups":
        return (await c.fetchJoinedChats()).map((chat) => chat.raw);
      case "bridge_owned_stickers": {
        const { ShopService } = await import(
          "https://jsr.io/@evex/linejs/3.5.0/base/service/shop/mod.ts"
        );
        const shop = new ShopService(c.base);
        return await shop.getOwnedProductSummaries(
          {
            shopId: "stickershop",
            offset: 0,
            limit: 80,
            locale: { language: "en", country: "TH" },
          } as never,
        );
      }
      case "bridge_send_text": {
        const raw = await c.base.talk.sendMessage({
          to: String(p.chatMid),
          text: String(p.text),
          relatedMessageId: p.relatedMessageId as string | undefined,
          e2ee: p.e2ee !== false,
        });
        return {
          rawMessage: packRaw({
            ...raw,
            text: String(p.text),
            relatedMessageId: p.relatedMessageId,
            messageRelationType: p.relatedMessageId
              ? "REPLY"
              : raw.messageRelationType,
          }),
        };
      }
      case "bridge_send_sticker": {
        const raw = await c.base.talk.sendMessage({
          to: String(p.chatMid),
          contentType: "STICKER",
          contentMetadata: {
            STKID: String(p.stickerId),
            STKPKGID: String(p.packageId),
            STKVER: String(p.version ?? "1"),
          },
          e2ee: false,
        });
        return { rawMessage: packRaw(raw) };
      }
      case "bridge_mark_read":
        await c.base.talk.sendChatChecked({
          chatMid: String(p.chatMid),
          lastMessageId: String(p.lastMessageId),
          seq: await c.base.getReqseq(),
        });
        return { marked: true };
      case "bridge_unsend":
        await c.base.talk.unsendMessage({
          messageId: String(p.messageId),
          seq: await c.base.getReqseq(),
        });
        return { chatMid: p.chatMid, messageId: p.messageId };
      case "bridge_find_contact":
        return await c.base.talk.findContactByUserid(
          { userid: String(p.userid) } as never,
        );
      case "bridge_friend_request":
        return await c.base.talk.tryFriendRequest(
          {
            mid: String(p.mid),
            method: "USERID",
            friendRequestParams: String(p.userid),
          } as never,
        );
      case "bridge_mute":
        await c.base.talk.updateContactSetting({
          reqSeq: await c.base.getReqseq(),
          mid: String(p.mid),
          flag: "CONTACT_SETTING_NOTIFICATION_DISABLE",
          value: p.muted ? "true" : "false",
        });
        return { mid: p.mid, muted: p.muted };
      case "bridge_postback":
        await c.base.talk.sendPostback({
          request: {
            messageId: String(p.messageId),
            url: String(
              p.uri ||
                `line://postback?data=${encodeURIComponent(String(p.data))}`,
            ),
            chatMID: String(p.chatMid),
            originMID: c.base.profile!.mid,
          },
        });
        return { sent: true };
      case "bridge_download": {
        const m = p.message as Json;
        const tm = await TalkMessage.fromRawTalk(
          unpackRaw(m.rawMessage) as Parameters<
            typeof TalkMessage.fromRawTalk
          >[0],
          c,
        );
        const blob = await tm.getData(false);
        const name = `${crypto.randomUUID()}.part`;
        const path = transferPath(name);
        const file = await Deno.open(path, {
          createNew: true,
          write: true,
          mode: 0o600,
        });
        try {
          await blob.stream().pipeTo(file.writable);
        } catch (error) {
          await Deno.remove(path).catch(() => {});
          throw error;
        }
        return { path, mime: blob.type, bytes: blob.size };
      }
      case "bridge_upload_plain_media": {
        const bytes = await Deno.readFile(transferPath(p.transferName));
        const data = new Blob([bytes], {
          type: String(p.mime || "application/octet-stream"),
        });
        const type = String(p.oType);
        if (!["image", "gif", "video", "audio", "file"].includes(type)) {
          throw new Error("invalid_media_type");
        }
        // Destination selection belongs to Rust. This call also sends the OBS
        // message; never follow it with sendMessage (which would duplicate it).
        const uploaded = await c.base.obs.uploadObjTalk(
          String(p.chatMid),
          type as "image" | "gif" | "video" | "audio" | "file",
          data,
          undefined,
          String(p.fileName),
          p.durationMs as number | undefined,
        );
        return { objId: uploaded.objId, objHash: uploaded.objHash };
      }
      case "bridge_send_media": {
        const bytes = await Deno.readFile(transferPath(p.transferName));
        const data = new Blob([bytes], {
          type: String(p.mime || "application/octet-stream"),
        });
        const preview = p.previewName
          ? new Blob([await Deno.readFile(transferPath(p.previewName))], {
            type: "image/jpeg",
          })
          : undefined;
        const type = String(p.oType);
        if (
          !String(p.chatMid).startsWith("u") &&
          !String(p.chatMid).startsWith("c")
        ) {
          throw new Error("e2ee_media_mid_unsupported");
        }
        const types: Record<string, [string, number]> = {
          image: ["emi", 1],
          gif: ["emi", 1],
          video: ["emv", 2],
          audio: ["ema", 3],
          file: ["emf", 14],
        };
        const [namespace, contentType] = types[type] ?? types.file;
        const encrypted = await c.base.e2ee.encryptByKeyMaterial(
          Buffer.from(await data.arrayBuffer()),
        );
        const encryptedBlob = new Blob([
          new Uint8Array(encrypted.encryptedData),
        ]);
        const params: Record<string, string> = { type: "file" };
        if (type === "gif") params.cat = "original";
        const upload = await c.base.obs.uploadObjectForService({
          data: encryptedBlob,
          oType: "file",
          obsPath: `talk/${namespace}/reqid-${crypto.randomUUID()}`,
          params,
        });
        if (["image", "gif", "video"].includes(type)) {
          const thumbnail = preview
            ? new Blob([
              new Uint8Array(
                (await c.base.e2ee.encryptByKeyMaterial(
                  Buffer.from(await preview.arrayBuffer()),
                  Buffer.from(encrypted.keyMaterial, "base64"),
                )).encryptedData,
              ),
            ])
            : encryptedBlob;
          await c.base.obs.uploadObjectForService({
            data: thumbnail,
            oType: "file",
            obsPath: `talk/${namespace}/${upload.objId}__ud-preview`,
            params,
          });
        }
        const chunks = await c.base.e2ee.encryptE2EEMessage(String(p.chatMid), {
          keyMaterial: encrypted.keyMaterial,
          fileName: String(p.fileName),
        }, contentType as never);
        const metadata: Record<string, string> = {
          SID: namespace,
          OID: upload.objId,
          FILE_SIZE: String(encryptedBlob.size),
          e2eeVersion: "2",
        };
        if (p.durationMs) metadata.DURATION = String(p.durationMs);
        if (type === "file") metadata.FILE_NAME = String(p.fileName);
        if (["image", "gif", "video"].includes(type)) {
          metadata.MEDIA_CONTENT_INFO = JSON.stringify({
            category: "original",
            fileSize: encryptedBlob.size,
            extension: String(p.extension),
            animated: type === "gif",
          });
        }
        const raw = await c.base.talk.sendMessage({
          to: String(p.chatMid),
          chunks,
          contentType: contentType as never,
          contentMetadata: metadata,
        });
        return { rawMessage: packRaw(raw) };
      }
      default:
        throw new Error(`unknown_protocol_method:${method}`);
    }
  }
  return async (method: string, p: Json) => {
    if (busy) throw new Error("bridge_busy");
    busy = true;
    try {
      return await rpc(method, p);
    } finally {
      busy = false;
    }
  };
}
