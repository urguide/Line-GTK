import { assertEquals } from "@std/assert";
import type { Client } from "@evex/linejs";
import { type CommandRuntime, createCommandService } from "../src/commands.ts";
import { replyTargetId } from "../src/messages.ts";

function harness(failures = 0) {
  const requests: Record<string, unknown>[] = [];
  const successes: unknown[] = [];
  const errors: string[] = [];
  const previews: unknown[] = [];
  const client = {
    base: {
      talk: {
        sendMessage: (request: Record<string, unknown>) => {
          requests.push(request);
          if (requests.length <= failures) {
            return Promise.reject(new Error("send rejected"));
          }
          return Promise.resolve({ id: "123", createdTime: 1 });
        },
      },
    },
  } as unknown as Client;
  const runtime = {
    getClient: () => client,
    msgCache: new Map(),
    setChatCache: () => {},
    sentMessagePayload: (_sent: unknown, to: string, text: string) => ({
      id: "123",
      to,
      text,
    }),
    touchChatPreviewFromMessage: (message: unknown) => previews.push(message),
    ok: (_id: unknown, result: unknown) => successes.push(result),
    fail: (_id: unknown, error: string) => errors.push(error),
  } as unknown as CommandRuntime;
  return {
    service: createCommandService(runtime),
    requests,
    successes,
    errors,
    previews,
  };
}

Deno.test("reply sends the exact target ID through E2EE and returns it to the UI", async () => {
  const h = harness();
  const target = "9007199254740993";
  await h.service.sendMessage(1, "c-group", "reply", target);
  assertEquals(h.requests, [{
    to: "c-group",
    text: "reply",
    relatedMessageId: target,
    e2ee: true,
  }]);
  assertEquals(h.successes, [{
    message: {
      id: "123",
      to: "c-group",
      text: "reply",
      relatedMessageId: target,
    },
  }]);
  assertEquals(h.errors, []);
});

Deno.test("reply relation survives the existing non-E2EE fallback", async () => {
  const h = harness(1);
  await h.service.sendMessage(1, "u-peer", "reply", "42");
  assertEquals(h.requests.map((r) => [r.relatedMessageId, r.e2ee]), [
    ["42", true],
    ["42", false],
  ]);
  assertEquals(h.successes.length, 1);
});

Deno.test("rejected replies do not report success or update the chat preview", async () => {
  const h = harness(2);
  await h.service.sendMessage(1, "u-peer", "reply", "42");
  assertEquals(h.successes, []);
  assertEquals(h.previews, []);
  assertEquals(h.errors, ["send rejected"]);
});

Deno.test("ordinary messages have no reply relation and pending targets are rejected", async () => {
  const h = harness();
  await h.service.sendMessage(1, "u-peer", "hello");
  assertEquals(h.requests[0].relatedMessageId, undefined);
  await h.service.sendMessage(2, "u-peer", "reply", "pending-1");
  await h.service.sendMessage(3, "u-peer", "reply", "");
  assertEquals(h.requests.length, 1);
  assertEquals(h.errors, ["invalid_reply_target", "invalid_reply_target"]);
});

Deno.test("incoming replies handle enum and numeric types without treating forwards as replies", () => {
  for (const messageRelationType of ["REPLY", 3]) {
    assertEquals(
      replyTargetId({
        relatedMessageId: "9007199254740993",
        messageRelationType,
      }),
      "9007199254740993",
    );
  }
  assertEquals(replyTargetId({}), null);
  assertEquals(replyTargetId({ messageRelationType: "REPLY" }), null);
  assertEquals(
    replyTargetId({
      relatedMessageId: "42",
      messageRelationType: "FORWARD",
    }),
    null,
  );
});
