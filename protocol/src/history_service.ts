import type { Client } from "@evex/linejs";
import { coerceI64 } from "./messages.ts";
import {
  type HistoryCursor,
  type HistoryMessage,
  MessageHistory,
  packRaw,
} from "./history.ts";
import { SerialQueue } from "./download_queue.ts";

export function createHistoryService(runtime: {
  getClient: () => Client | null;
  history: MessageHistory;
  summarize: (raw: unknown) => Promise<HistoryMessage>;
  forUi: (rows: HistoryMessage[]) => Promise<HistoryMessage[]>;
  hydrate: (rows: HistoryMessage[], chat: string) => Promise<void>;
  remember: (chat: string, rows: HistoryMessage[]) => void;
  memoryTtl?: () => number;
  pauseMs?: number;
  backfillPauseMs?: number;
  onBackfillError?: (chat: string, error: unknown) => void;
}) {
  const requests = new SerialQueue(runtime.pauseMs ?? 500);
  let generation = 0;
  const backfillChats = new Set<string>();
  let backfill: Promise<void> | null = null;
  const fresh = new Map<
    string,
    {
      at: number;
      rows: HistoryMessage[];
      cursor: HistoryCursor | null;
      hasMore: boolean;
    }
  >();

  const cursorOf = (row: HistoryMessage): HistoryCursor | null => {
    if (!/^\d+$/.test(String(row.id)) || row.deliveredTime == null) return null;
    return {
      messageId: String(row.id),
      deliveredTime: String(row.deliveredTime),
    };
  };

  async function fetchPage(
    chat: string,
    before?: HistoryCursor,
    background = false,
  ) {
    const session = runtime.getClient();
    const epoch = generation;
    const result = await requests.run(async () => {
      const client = runtime.getClient();
      if (epoch !== generation || client !== session) {
        throw new Error("session_changed");
      }
      if (!client) throw new Error("not_logged_in");
      if (!chat) throw new Error("missing_chat");
      if (
        before &&
        (!/^\d+$/.test(before.messageId) || !/^\d+$/.test(before.deliveredTime))
      ) {
        throw new Error("invalid_history_cursor");
      }
      let rows: HistoryMessage[];
      let nextCursor: HistoryCursor | null;
      let hasMore: boolean;
      let cached = false;
      const page = before ? await runtime.history.page(chat, before) : null;
      const recent = fresh.get(chat);
      if (page) {
        rows = page.messages;
        nextCursor = page.nextCursor;
        hasMore = page.hasMore;
        cached = true;
      } else if (
        !before && recent &&
        Date.now() - recent.at < (runtime.memoryTtl?.() ?? 20 * 60_000)
      ) {
        rows = recent.rows;
        nextCursor = recent.cursor;
        hasMore = recent.hasMore;
        cached = true;
      } else {
        let end = before;
        if (!end) {
          const boxes = await client.base.talk.getMessageBoxes({
            messageBoxListRequest: {},
          });
          const last = boxes.messageBoxes.find((box) => box.id === chat)
            ?.lastDeliveredMessageId;
          if (last) {
            end = {
              messageId: String(last.messageId),
              deliveredTime: String(last.deliveredTime),
            };
          }
        }
        if (!end) {
          return {
            messages: [],
            nextCursor: null,
            hasMore: false,
            cached: false,
          };
        }
        const raw = await client.base.talk.getPreviousMessagesV2WithRequest({
          request: {
            messageBoxId: chat,
            endMessageId: {
              messageId: coerceI64(end.messageId),
              deliveredTime: coerceI64(end.deliveredTime),
            },
            messagesCount: 20,
          },
        });
        rows = [];
        for (const message of raw) {
          const summary = await runtime.summarize(message);
          rows.push({
            ...summary,
            deliveredTime: String(message.deliveredTime),
            rawMessage: packRaw(message),
          });
        }
        const oldest = raw.at(-1);
        nextCursor = oldest
          ? cursorOf({ id: oldest.id, deliveredTime: oldest.deliveredTime })
          : null;
        hasMore = raw.length === 20 && nextCursor !== null &&
          nextCursor.messageId !== before?.messageId;
        // Servers may repeat the boundary row. It is already on screen.
        rows = rows.filter((row) => String(row.id) !== before?.messageId);
        if (!rows.length) hasMore = false;
        if (epoch !== generation || runtime.getClient() !== client) {
          throw new Error("session_changed");
        }
        const all = await runtime.history.save(chat, rows, before, {
          ids: rows.map((row) => String(row.id)),
          nextCursor,
          hasMore,
        }, !before);
        runtime.remember(chat, all);
        if (!before) {
          fresh.set(chat, {
            at: Date.now(),
            rows,
            cursor: nextCursor,
            hasMore,
          });
        }
      }
      if (runtime.getClient() !== client) throw new Error("session_changed");
      // Resolve from the merged disk records so a fresh server summary cannot
      // discard paths that a previous download has already materialized.
      const ids = new Set(rows.map((row) => String(row.id)));
      const persisted = await runtime.history.load(chat);
      const ui = await runtime.forUi(
        persisted.filter((row) => ids.has(String(row.id))),
      );
      if (!background) {
        void runtime.hydrate(ui, chat).catch((error) =>
          console.error("[history media]", error)
        );
      }
      return { messages: ui, nextCursor, hasMore, cached };
    });
    if (!before && epoch === generation) scheduleBackfill(chat);
    return result;
  }

  function scheduleBackfill(chat: string) {
    backfillChats.add(chat);
    if (backfill) return;
    const epoch = generation;
    backfill = runBackfill(epoch).finally(() => {
      backfill = null;
      // A new session can enqueue work while the previous worker is stopping.
      const next = backfillChats.values().next().value;
      if (next) scheduleBackfill(next);
    });
  }

  async function runBackfill(epoch: number) {
    while (backfillChats.size && epoch === generation) {
      const chat = backfillChats.values().next().value!;
      backfillChats.delete(chat);
      try {
        const entry = Object.entries(await runtime.history.gaps(chat))[0];
        if (!entry || epoch !== generation) continue;
        const [key, gap] = entry;
        const pause = runtime.backfillPauseMs ?? 1000;
        if (pause) await new Promise((resolve) => setTimeout(resolve, pause));
        if (epoch !== generation) return;
        const page = await fetchPage(chat, gap.before, true);
        if (epoch !== generation) return;
        // Wait for this page's serial media work as well, so background history
        // cannot build an unbounded image/video download backlog.
        await runtime.hydrate(page.messages, chat);
        if (epoch !== generation) return;
        const connected = gap.before.messageId === gap.targetId ||
          page.messages.some((row) => String(row.id) === gap.targetId);
        const done = connected || !page.hasMore || !page.nextCursor ||
          page.nextCursor.messageId === gap.before.messageId;
        await runtime.history.advanceGap(
          chat,
          key,
          done ? null : page.nextCursor,
        );
        backfillChats.add(chat);
      } catch (error) {
        // Keep the disk checkpoint. Retry on a later open, not in a hot loop.
        if (epoch === generation) {
          if (runtime.onBackfillError) runtime.onBackfillError(chat, error);
          else console.error("[history backfill]", chat, error);
        }
      }
    }
  }

  return {
    fetchPage,
    waitForBackfill: async () => {
      while (backfill) await backfill;
    },
    invalidate: (chat: string) => fresh.delete(chat),
    clear: () => {
      generation++;
      fresh.clear();
      backfillChats.clear();
    },
  };
}
