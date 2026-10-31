import type { Client } from "@evex/linejs";
import { atomicWriteJson } from "./storage.ts";

type Json = Record<string, unknown>;

/** LINE uses param1 for the chat and param2 for the message, including groups. */
export function unsendFromOperation(op: {
  type?: string | number;
  param1?: string;
  param2?: string;
}): { chatMid: string; messageId: string } | null {
  if (
    !["DESTROY_MESSAGE", "NOTIFIED_DESTROY_MESSAGE", "64", "65"].includes(
      String(op.type),
    )
  ) return null;
  const chatMid = op.param1 ?? "";
  const messageId = op.param2 ?? "";
  return chatMid && messageId ? { chatMid, messageId } : null;
}

/** Tombstones prevent old disk caches and in-flight fetches restoring messages. */
export class UnsentMessages {
  private ids = new Set<string>();
  private writing: Promise<void> = Promise.resolve();

  constructor(private path: string) {}

  async load(): Promise<void> {
    try {
      const ids: unknown = JSON.parse(await Deno.readTextFile(this.path));
      if (!Array.isArray(ids) || ids.some((id) => typeof id !== "string")) {
        throw new Error("invalid unsent message cache");
      }
      for (const id of ids) this.ids.add(id);
    } catch (error) {
      if (!(error instanceof Deno.errors.NotFound)) throw error;
    }
  }

  has(id: unknown): boolean {
    return this.ids.has(String(id ?? ""));
  }

  filter(messages: Json[]): Json[] {
    return messages.filter((message) => !this.has(message.id));
  }

  mark(messageId: string): Promise<void> {
    this.ids.add(messageId);
    // Serialize snapshots: concurrent events must not overwrite newer IDs.
    this.writing = this.writing.catch(() => {}).then(() =>
      atomicWriteJson(this.path, [...this.ids])
    );
    return this.writing;
  }
}

export function createUnsendService(runtime: {
  getClient: () => Client | null;
  applyUnsent: (chatMid: string, messageId: string) => Promise<void>;
}) {
  return async (chatMid: string, messageId: string): Promise<void> => {
    const client = runtime.getClient();
    if (!client) throw new Error("not_logged_in");
    if (!chatMid || !/^\d+$/.test(messageId)) {
      throw new Error("invalid_message_id");
    }
    // Ownership and the current unsend time limit are enforced by LINE.
    // Keep IDs as strings (they can exceed JavaScript's safe integer range).
    await client.base.talk.unsendMessage({
      seq: await client.base.getReqseq(),
      messageId,
    });
    await runtime.applyUnsent(chatMid, messageId);
  };
}
