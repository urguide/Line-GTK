import { assertEquals, assertRejects } from "@std/assert";
import type { Client } from "@evex/linejs";
import { createBridge } from "../src/bridge.ts";

Deno.test("bridge performs exactly one 20-message request with lossless cursor and no background work", async () => {
  let calls = 0;
  const client = {
    base: {
      talk: {
        getPreviousMessagesV2WithRequest: (
          args: {
            request: {
              messagesCount: number;
              endMessageId: { messageId: bigint | number };
            };
          },
        ) => {
          calls++;
          assertEquals(args.request.messagesCount, 20);
          assertEquals(args.request.endMessageId.messageId, 9007199254740993n);
          return Promise.resolve([{
            id: "9007199254740992",
            deliveredTime: 100,
            createdTime: 100,
            chunks: [new Uint8Array([1, 2])],
          }]);
        },
      },
    },
  } as unknown as Client;
  const bridge = createBridge({
    client: () => client,
    summarize: (raw) => Promise.resolve(raw as Record<string, unknown>),
    transferDir: "/unused",
  });
  const result = await bridge.fetchPage("chat", {
    messageId: "9007199254740993",
    deliveredTime: "101",
  });
  assertEquals(calls, 1);
  assertEquals(result.nextCursor?.messageId, "9007199254740992");
  assertEquals(result.hasMore, false);
  assertEquals(result.messages.length, 1);
  await assertRejects(
    () => bridge.download({ id: "../bad" }),
    Error,
    "invalid_message_id",
  );
});
