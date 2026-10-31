import { assertEquals, assertRejects } from "@std/assert";
import type { Client } from "@evex/linejs";
import {
  createUnsendService,
  unsendFromOperation,
  UnsentMessages,
} from "../src/unsend.ts";

Deno.test("unsend operations use chat and message IDs, including own-device events", () => {
  for (const type of ["DESTROY_MESSAGE", "NOTIFIED_DESTROY_MESSAGE", 64, 65]) {
    assertEquals(
      unsendFromOperation({
        type,
        param1: "c-group",
        param2: "9007199254740993",
      }),
      { chatMid: "c-group", messageId: "9007199254740993" },
    );
  }
  assertEquals(unsendFromOperation({ type: 65, param1: "u-peer" }), null);
  assertEquals(
    unsendFromOperation({ type: 55, param1: "u-peer", param2: "42" }),
    null,
  );
});

Deno.test("unsend waits for server success and preserves large string IDs", async () => {
  const calls: unknown[] = [];
  let accept!: () => void;
  const server = new Promise<void>((resolve) => accept = resolve);
  const client = {
    base: {
      getReqseq: () => Promise.resolve(7),
      talk: {
        unsendMessage: (request: unknown) => {
          calls.push(request);
          return server;
        },
      },
    },
  } as unknown as Client;
  const unsend = createUnsendService({
    getClient: () => client,
    applyUnsent: (chat, id) => {
      calls.push([chat, id]);
      return Promise.resolve();
    },
  });
  const pending = unsend("c-group", "9007199254740993");
  await Promise.resolve();
  assertEquals(calls, [{ seq: 7, messageId: "9007199254740993" }]);
  accept();
  await pending;
  assertEquals(calls[1], ["c-group", "9007199254740993"]);
});

Deno.test("rejected and invalid unsends never remove the local message", async () => {
  let applied = false;
  let requested = 0;
  const client = {
    base: {
      getReqseq: () => Promise.resolve(1),
      talk: {
        unsendMessage: () => {
          requested++;
          return Promise.reject(new Error("UNSEND_TIME_LIMIT"));
        },
      },
    },
  } as unknown as Client;
  const unsend = createUnsendService({
    getClient: () => client,
    applyUnsent: () => {
      applied = true;
      return Promise.resolve();
    },
  });
  await assertRejects(() => unsend("u-peer", "42"), Error, "UNSEND_TIME_LIMIT");
  await assertRejects(() => unsend("u-peer", "pending-42"), Error, "invalid");
  await assertRejects(() => unsend("", "42"), Error, "invalid");
  assertEquals(requested, 1);
  assertEquals(applied, false);
  const offline = createUnsendService({
    getClient: () => null,
    applyUnsent: () => Promise.resolve(),
  });
  await assertRejects(() => offline("u-peer", "42"), Error, "not_logged_in");
});

Deno.test("tombstones survive restart and concurrent events filter stale results", async () => {
  const directory = await Deno.makeTempDir({ prefix: "line-gtk-unsent-" });
  try {
    const path = `${directory}/unsent.json`;
    const cache = new UnsentMessages(path);
    await cache.load();
    const stale = [{ id: "41", text: "keep" }, { id: "42", text: "remove" }];
    const first = cache.mark("42");
    // Visible immediately, even before disk persistence finishes.
    assertEquals(cache.filter(stale), [stale[0]]);
    await Promise.all([first, cache.mark("43"), cache.mark("42")]);
    const restored = new UnsentMessages(path);
    await restored.load();
    assertEquals(restored.filter([...stale, { id: "43" }]), [stale[0]]);
    assertEquals(cache.has(undefined), false);
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});
