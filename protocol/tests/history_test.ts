import { assertEquals, assertRejects } from "@std/assert";
import {
  mergeMessages,
  MessageHistory,
  packRaw,
  unpackRaw,
} from "../src/history.ts";
import { SerialQueue } from "../src/download_queue.ts";
import { createHistoryService } from "../src/history_service.ts";
import type { Client } from "@evex/linejs";

Deno.test("history merges concurrent pages and media paths without losing old messages", async () => {
  const directory = await Deno.makeTempDir();
  try {
    const history = new MessageHistory(
      directory,
      () => 360 * 86400000,
      (rows) => rows,
    );
    await Promise.all([
      history.save("chat", [{
        id: "9007199254740993",
        createdTime: 1,
        text: "old",
      }]),
      history.save("chat", [{
        id: "9007199254740994",
        createdTime: 1,
        text: "new",
      }]),
      history.save("chat", [{
        id: "9007199254740993",
        imagePath: "/cache/image",
      }]),
    ]);
    await history.save("chat", [{
      id: "9007199254740993",
      imagePath: null,
      text: "updated",
    }]);
    const rows = await history.load("chat");
    assertEquals(rows.length, 2);
    assertEquals(rows[0].id, "9007199254740993");
    assertEquals(rows[0].imagePath, "/cache/image");
    assertEquals(rows[0].text, "updated");
    assertEquals(
      await history.page("chat", { messageId: "42", deliveredTime: "1" }),
      null,
    );
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("raw E2EE chunks and 64-bit identifiers survive disk serialization", () => {
  const raw = {
    id: 9007199254740993n,
    chunks: [new Uint8Array([0, 128, 255])],
  };
  const restored = unpackRaw(packRaw(raw)) as {
    id: string;
    chunks: Uint8Array[];
  };
  assertEquals(restored.id, "9007199254740993");
  assertEquals([...restored.chunks[0]], [...raw.chunks[0]]);
  assertEquals(mergeMessages([], [{ id: "" }]), []);
});

Deno.test("shared download lane is serial and survives a failed download", async () => {
  const queue = new SerialQueue();
  let active = 0, peak = 0;
  const results = await Promise.allSettled(
    Array.from({ length: 8 }, (_, i) =>
      queue.run(async () => {
        active++;
        peak = Math.max(peak, active);
        await new Promise((resolve) => setTimeout(resolve, 1));
        active--;
        if (i === 2) throw new Error("download failed");
        return i;
      })),
  );
  assertEquals(peak, 1);
  assertEquals(results[2].status, "rejected");
  assertEquals(results[7].status, "fulfilled");
});

Deno.test("20-message pages preserve cursors, reuse disk after restart and stop at end", async () => {
  const directory = await Deno.makeTempDir();
  try {
    const history = new MessageHistory(
      directory,
      () => 86400000,
      (rows) => rows,
    );
    const base = 9007199254740993n;
    let calls = 0;
    const client = {
      base: {
        talk: {
          getMessageBoxes: () =>
            Promise.resolve({
              messageBoxes: [{
                id: "chat",
                lastDeliveredMessageId: {
                  messageId: base + 39n,
                  deliveredTime: 40,
                },
              }],
            }),
          getPreviousMessagesV2WithRequest: (
            { request }: {
              request: {
                messagesCount: number;
                endMessageId: { messageId: bigint | number };
              };
            },
          ) => {
            calls++;
            assertEquals(request.messagesCount, 20);
            const end = BigInt(request.endMessageId.messageId);
            const count = end === base + 39n ? 20 : end === base + 20n ? 20 : 0;
            const start = end === base + 39n ? 39 : 19;
            return Promise.resolve(
              Array.from(
                { length: count },
                (_, i) => ({
                  id: String(base + BigInt(start - i)),
                  deliveredTime: start - i + 1,
                  createdTime: start - i + 1,
                }),
              ),
            );
          },
        },
      },
    } as unknown as Client;
    const runtime = {
      getClient: () => client,
      history,
      pauseMs: 0,
      summarize: (raw: unknown) =>
        Promise.resolve(raw as Record<string, unknown>),
      forUi: (rows: Record<string, unknown>[]) => Promise.resolve(rows),
      hydrate: () => Promise.resolve(),
      remember: () => {},
    };
    const service = createHistoryService(runtime);
    const first = await service.fetchPage("chat");
    await service.waitForBackfill();
    assertEquals(first.messages.length, 20);
    assertEquals(first.nextCursor?.messageId, String(base + 20n));
    assertEquals(first.hasMore, true);
    const second = await service.fetchPage("chat", first.nextCursor!);
    assertEquals(second.messages.length, 20);
    assertEquals((await history.load("chat")).length, 40);
    const restart = createHistoryService(runtime);
    assertEquals(
      (await restart.fetchPage("chat", first.nextCursor!)).cached,
      true,
    );
    assertEquals(calls, 2);
    const end = await restart.fetchPage("chat", second.nextCursor!);
    assertEquals(end.hasMore, false);
    assertEquals(end.messages, []);
    assertEquals(
      (await restart.fetchPage("chat", second.nextCursor!)).cached,
      true,
    );
    assertEquals(calls, 3);
    const queued = restart.fetchPage("chat");
    restart.clear();
    await assertRejects(() => queued, Error, "session_changed");
    await restart.waitForBackfill();
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("background fills 101..130 without scrolling, serially, and resumes after failure", async () => {
  const directory = await Deno.makeTempDir();
  try {
    const history = new MessageHistory(
      directory,
      () => 86400000,
      (rows) => rows,
    );
    const row = (id: number) => ({
      id: String(id),
      createdTime: id,
      deliveredTime: id,
      contentType: "IMAGE",
    });
    await history.save(
      "chat",
      Array.from({ length: 100 }, (_, i) => row(i + 1)),
    );
    const requests: number[] = [];
    const hydrated: number[][] = [];
    let fail = true;
    let active = 0;
    let peak = 0;
    const client = {
      base: {
        talk: {
          getMessageBoxes: () =>
            Promise.resolve({
              messageBoxes: [{
                id: "chat",
                lastDeliveredMessageId: { messageId: 150, deliveredTime: 150 },
              }],
            }),
          getPreviousMessagesV2WithRequest: async (
            { request }: {
              request: {
                messagesCount: number;
                endMessageId: { messageId: number | bigint };
              };
            },
          ) => {
            assertEquals(request.messagesCount, 20);
            const cursor = Number(request.endMessageId.messageId);
            requests.push(cursor);
            active++;
            peak = Math.max(peak, active);
            await Promise.resolve();
            active--;
            if (fail && cursor === 111) throw new Error("temporary failure");
            const start = cursor === 150 ? 150 : cursor - 1;
            return Array.from(
              { length: Math.min(20, start) },
              (_, i) => row(start - i),
            );
          },
        },
      },
    } as unknown as Client;
    const runtime = {
      history,
      getClient: () => client,
      pauseMs: 0,
      backfillPauseMs: 0,
      summarize: (raw: unknown) =>
        Promise.resolve(raw as Record<string, unknown>),
      forUi: (rows: Record<string, unknown>[]) => Promise.resolve(rows),
      hydrate: (rows: Record<string, unknown>[]) => {
        hydrated.push(rows.map((r) => Number(r.id)));
        return Promise.resolve();
      },
      remember: () => {},
      onBackfillError: () => {},
    };
    const service = createHistoryService(runtime);
    const initial = await service.fetchPage("chat");
    assertEquals(initial.messages.length, 20);
    assertEquals(initial.messages[0].id, "131");
    await service.waitForBackfill();
    assertEquals(requests, [150, 131, 111]);
    assertEquals(
      Object.values(await history.gaps("chat"))[0].before.messageId,
      "111",
    );
    fail = false;
    const restart = createHistoryService(runtime);
    await restart.fetchPage("chat");
    await restart.waitForBackfill();
    assertEquals(
      (await history.load("chat")).map((m) => Number(m.id)),
      Array.from({ length: 150 }, (_, i) => i + 1),
    );
    assertEquals(await history.gaps("chat"), {});
    assertEquals(peak, 1);
    assertEquals(hydrated.some((page) => page.includes(101)), true);
    // Migration deliberately bridges to the oldest known point (1), not just
    // the first overlap (100), since old cache files may contain gaps already.
    assertEquals(requests.filter((cursor) => cursor === 131).length, 1);
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("background waits for page media and stops on session clear", async () => {
  const directory = await Deno.makeTempDir();
  const mediaStarted = Promise.withResolvers<void>();
  const mediaFinished = Promise.withResolvers<void>();
  try {
    const history = new MessageHistory(
      directory,
      () => 86400000,
      (rows) => rows,
    );
    await history.save("chat", [{ id: "100", createdTime: 100 }]);
    const requested: number[] = [];
    const client = {
      base: {
        talk: {
          getMessageBoxes: () =>
            Promise.resolve({
              messageBoxes: [{
                id: "chat",
                lastDeliveredMessageId: { messageId: 150, deliveredTime: 150 },
              }],
            }),
          getPreviousMessagesV2WithRequest: (
            { request }: {
              request: { endMessageId: { messageId: number | bigint } };
            },
          ) => {
            const cursor = Number(request.endMessageId.messageId);
            requested.push(cursor);
            const start = cursor === 150 ? 150 : cursor - 1;
            return Promise.resolve(
              Array.from(
                { length: 20 },
                (_, i) => ({
                  id: String(start - i),
                  createdTime: start - i,
                  deliveredTime: start - i,
                }),
              ),
            );
          },
        },
      },
    } as unknown as Client;
    const service = createHistoryService({
      history,
      getClient: () => client,
      pauseMs: 0,
      backfillPauseMs: 0,
      summarize: (raw) => Promise.resolve(raw as Record<string, unknown>),
      forUi: (rows) => Promise.resolve(rows),
      remember: () => {},
      hydrate: async (rows) => {
        if (rows.some((row) => row.id === "130")) {
          mediaStarted.resolve();
          await mediaFinished.promise;
        }
      },
    });
    const first = await service.fetchPage("chat");
    assertEquals(first.messages.length, 20);
    await mediaStarted.promise;
    assertEquals(requested, [150, 131]);
    service.clear();
    mediaFinished.resolve();
    await service.waitForBackfill();
    assertEquals(requested, [150, 131]);
    // Keep the checkpoint until media has finished in the same session.
    assertEquals(
      Object.values(await history.gaps("chat"))[0].before.messageId,
      "131",
    );
  } finally {
    mediaFinished.resolve();
    await Deno.remove(directory, { recursive: true });
  }
});
