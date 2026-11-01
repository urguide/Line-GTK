/** linejs 3.5 exposes nested read ranges as numeric Thrift fields. */
type Json = Record<string, unknown>;
export function normalizeReadRanges(value: unknown): unknown[] {
  if (!Array.isArray(value)) throw new Error("invalid_read_ranges");
  return value.map((chat: Json) => {
    if (
      typeof chat.chatId !== "string" || !chat.ranges ||
      typeof chat.ranges !== "object"
    ) {
      throw new Error("invalid_read_ranges");
    }
    const ranges: Record<string, unknown[]> = {};
    for (const [reader, entries] of Object.entries(chat.ranges)) {
      if (!entries || typeof entries !== "object") {
        throw new Error("invalid_read_ranges");
      }
      ranges[reader] = Object.values(entries).map((value) => {
        if (!value || typeof value !== "object") {
          throw new Error("invalid_read_ranges");
        }
        const entry = value as Json;
        const startMessageId = String(entry.startMessageId ?? entry[1] ?? "");
        const endMessageId = String(entry.endMessageId ?? entry[2] ?? "");
        if (!/^\d+$/.test(startMessageId) || !/^\d+$/.test(endMessageId)) {
          throw new Error("invalid_read_ranges");
        }
        return { startMessageId, endMessageId };
      });
    }
    return { chatId: chat.chatId, ranges };
  });
}

export function readFromOperation(
  op: {
    type?: string | number;
    param1?: string;
    param2?: string;
    param3?: string;
  },
): { chatMid: string; readerMid: string; messageId: string } | null {
  if (!["NOTIFIED_READ_MESSAGE", "55"].includes(String(op.type))) return null;
  const { param1: chatMid, param2: readerMid, param3: messageId } = op;
  if (!chatMid || !readerMid || !messageId || !/^\d+$/.test(messageId)) {
    return null;
  }
  return { chatMid, readerMid, messageId };
}
