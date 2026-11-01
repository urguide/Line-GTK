/** LINE video originals use chunk-hash MACs, unlike images and thumbnails. */
import type { Client, TalkMessage } from "@evex/linejs";
import { Buffer } from "node:buffer";
import {
  createDecipheriv,
  createHash,
  createHmac,
  hkdfSync,
  timingSafeEqual,
} from "node:crypto";

export function decryptVideo(
  data: Uint8Array,
  keyMaterial: string,
): Uint8Array<ArrayBuffer> {
  const key = Buffer.from(keyMaterial, "base64");
  if (key.length !== 32) throw new Error("invalid_video_key_material");
  if (data.length < 32) {
    throw new Error("encrypted video too short to contain HMAC");
  }
  const derived = Buffer.from(
    hkdfSync("sha256", key, new Uint8Array(), "FileEncryption", 76),
  );
  const ciphertext = data.subarray(0, -32);
  const tag = data.subarray(-32);
  const macKey = derived.subarray(32, 64);
  const mac = createHmac("sha256", macKey);
  // LINE Encryption Overview, Video Encryption: SHA-256 per 128 KiB,
  // then HMAC over the concatenated hashes (not over the ciphertext).
  for (let offset = 0; offset < ciphertext.length; offset += 128 * 1024) {
    mac.update(
      createHash("sha256").update(
        ciphertext.subarray(offset, offset + 128 * 1024),
      ).digest(),
    );
  }
  if (!timingSafeEqual(tag, mac.digest())) {
    // Older linejs uploads use the file-style MAC even for video. Accept
    // those only after authenticating the entire ciphertext as well.
    const legacy = createHmac("sha256", macKey).update(ciphertext).digest();
    if (!timingSafeEqual(tag, legacy)) {
      throw new Error(
        "HMAC verification failed: encrypted video is damaged or its key is incorrect",
      );
    }
  }
  const nonce = Buffer.concat([derived.subarray(64, 76), Buffer.alloc(4)]);
  const decipher = createDecipheriv(
    "aes-256-ctr",
    derived.subarray(0, 32),
    nonce,
  );
  return new Uint8Array(
    Buffer.concat([decipher.update(ciphertext), decipher.final()]),
  );
}

export async function downloadMedia(
  client: Client,
  message: TalkMessage,
): Promise<Blob> {
  const raw = message.raw;
  if (
    !["VIDEO", "2"].includes(String(raw.contentType)) ||
    !raw.chunks?.length || raw.contentMetadata?.DOWNLOAD_URL
  ) return await message.getData(false);

  if (!/^[uc]/.test(raw.to)) throw new Error("invalid_video_destination");
  const { keyMaterial, fileName } = await client.base.e2ee
    .decryptE2EEDataMessage(raw);
  if (typeof keyMaterial !== "string") {
    throw new Error("invalid_video_key_material");
  }
  const { SID, OID } = raw.contentMetadata;
  if (!SID || !OID) throw new Error("missing_video_object_metadata");
  // Same X-Talk-Meta as linejs: TBinaryProtocol Message with string field 4
  // (id), empty struct-list field 27 (chunks), and STOP. Preserve i64 IDs.
  const id = Buffer.from(String(raw.id));
  const header = Buffer.alloc(7);
  header[0] = 11;
  header.writeInt16BE(4, 1);
  header.writeInt32BE(id.length, 3);
  const talkMeta = Buffer.from(JSON.stringify({
    message: Buffer.concat([
      header,
      id,
      Buffer.from([15, 0, 27, 12, 0, 0, 0, 0, 0]),
    ]).toString("base64"),
  })).toString("base64");
  const encrypted = await client.base.obs.downloadObjectForService({
    obsPath: `talk/${SID}`,
    oid: OID,
    addHeaders: { "X-Talk-Meta": talkMeta },
  });
  const plaintext = decryptVideo(
    new Uint8Array(await encrypted.arrayBuffer()),
    keyMaterial,
  );
  return new File(
    [plaintext],
    typeof fileName === "string" ? fileName : "video.mp4",
  );
}
