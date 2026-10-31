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
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});
