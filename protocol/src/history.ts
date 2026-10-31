import { join } from "@std/path";
import { atomicWriteJson } from "./storage.ts";
import { SerialQueue } from "./download_queue.ts";
import { Buffer } from "node:buffer";

export type HistoryMessage = Record<string, unknown>;
export type HistoryCursor = { messageId: string; deliveredTime: string };
type Page = {
  ids: string[];
  nextCursor: HistoryCursor | null;
  hasMore: boolean;
};
type HistoryFile = {
  at: number;
  messages: HistoryMessage[];
  pages: Record<string, Page>;
};

export function mergeMessages(
  old: HistoryMessage[],
  incoming: HistoryMessage[],
): HistoryMessage[] {
  const byId = new Map<string, HistoryMessage>();
  for (const message of [...old, ...incoming]) {
    const id = String(message.id ?? "");
    if (!id) continue;
    const previous = byId.get(id);
    const merged: HistoryMessage = { ...previous, ...message, id };
    for (const key of ["imagePath", "filePath", "audioPath", "rawMessage"]) {
      if (merged[key] == null && previous?.[key] != null) {
        merged[key] = previous[key];
      }
    }
    byId.set(id, merged);
  }
  return [...byId.values()].sort((a, b) => {
    const time = Number(a.createdTime) - Number(b.createdTime);
    if (time) return time;
    const x = String(a.id), y = String(b.id);
    if (/^\d+$/.test(x) && /^\d+$/.test(y)) {
      return BigInt(x) < BigInt(y) ? -1 : BigInt(x) > BigInt(y) ? 1 : 0;
    }
    return x.localeCompare(y);
  });
}

/** Keep i64 IDs and encrypted media metadata lossless across restarts. */
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

/** Serial read/merge/write prevents live arrivals or media completions losing pages. */
export class MessageHistory {
  private writes = new SerialQueue();

  constructor(
    private readonly directory: string,
    private readonly ttl: () => number,
    private readonly filter: (rows: HistoryMessage[]) => HistoryMessage[],
  ) {}

  private async read(chat: string): Promise<HistoryFile> {
    try {
      const file = JSON.parse(
        await Deno.readTextFile(join(this.directory, `${chat}.json`)),
      );
      if (Date.now() - Number(file.at ?? 0) <= this.ttl()) {
        return {
          at: file.at,
          messages: this.filter(file.messages ?? []),
          pages: file.pages ?? {},
        };
      }
    } catch (error) {
      if (!(error instanceof Deno.errors.NotFound)) throw error;
    }
    return { at: Date.now(), messages: [], pages: {} };
  }

  async load(chat: string): Promise<HistoryMessage[]> {
    return (await this.read(chat)).messages;
  }

  async page(
    chat: string,
    before: HistoryCursor,
  ): Promise<(Page & { messages: HistoryMessage[] }) | null> {
    const file = await this.read(chat);
    const page = file.pages[before.messageId];
    if (!page) return null;
    const ids = new Set(page.ids);
    return {
      ...page,
      messages: file.messages.filter((m) => ids.has(String(m.id))),
    };
  }

  save(
    chat: string,
    messages: HistoryMessage[],
    before?: HistoryCursor,
    page?: Page,
  ): Promise<HistoryMessage[]> {
    return this.writes.run(async () => {
      const file = await this.read(chat);
      file.messages = this.filter(mergeMessages(file.messages, messages));
      // Only reuse pages fetched from this exact boundary; never assume that
      // disconnected cached ranges form continuous history.
      if (before && page) file.pages[before.messageId] = page;
      file.at = Date.now();
      await atomicWriteJson(join(this.directory, `${chat}.json`), file);
      return file.messages;
    });
  }
}
