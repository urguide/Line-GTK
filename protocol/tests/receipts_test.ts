import { assertEquals, assertThrows } from "@std/assert";
import type { Client, TalkMessage } from "@evex/linejs";
import { normalizeReadRanges, readFromOperation } from "../src/receipts.ts";
import { createApi } from "../src/api.ts";

Deno.test("read operations preserve reader identity and 64-bit IDs", () => {
  const op = {
    type: 55,
    param1: "c1",
    param2: "u1",
    param3: "9007199254740993",
  };
  assertEquals(readFromOperation(op), {
    chatMid: "c1",
    readerMid: "u1",
    messageId: op.param3,
  });
  assertEquals(
    readFromOperation({ ...op, type: "NOTIFIED_READ_MESSAGE" }),
    readFromOperation(op),
  );
  assertEquals(readFromOperation({ ...op, param2: undefined }), null);
  assertEquals(readFromOperation({ ...op, param3: "bad" }), null);
});

Deno.test("normalize linejs numeric nested structs and named ranges losslessly", () => {
  const range = {
    startMessageId: "9007199254740993",
    endMessageId: "9007199254740999",
  };
  assertEquals(
    normalizeReadRanges([{
      chatId: "c1",
      ranges: { u1: { 0: { 1: 9007199254740993n, 2: 9007199254740999n } } },
    }]),
    [{ chatId: "c1", ranges: { u1: [range] } }],
  );
  assertEquals(
    normalizeReadRanges([{ chatId: "c1", ranges: { u1: [range] } }]),
    [{ chatId: "c1", ranges: { u1: [range] } }],
  );
  assertThrows(() =>
    normalizeReadRanges([{ chatId: "c1", ranges: { u1: [{}] } }])
  );
});

Deno.test("history requests server counts and range RPC addresses only requested chat", async () => {
  const client = {
    base: {
      talk: {
        getPreviousMessagesV2WithRequest: (
          { request }: { request: { withReadCount: boolean } },
        ) => {
          assertEquals(request.withReadCount, true);
          return Promise.resolve([{
            id: "10",
            readCount: 4,
            deliveredTime: 10,
          }]);
        },
        getMessageReadRange: (p: unknown) => {
          assertEquals(p, { chatIds: ["c1"] });
          return Promise.resolve([{
            chatId: "c1",
            ranges: { u1: [{ startMessageId: 1n, endMessageId: 10n }] },
          }]);
        },
      },
    },
  } as unknown as Client;
  const api = createApi(
    () => client,
    "/unused",
    (raw) => Promise.resolve({ raw } as unknown as TalkMessage),
  );
  const page = await api("bridge_fetch_page", {
    chatMid: "c1",
    before: { messageId: "11", deliveredTime: "11" },
  }) as { messages: { rawMessage: { readCount: number } }[] };
  assertEquals(page.messages[0].rawMessage.readCount, 4);
  assertEquals(await api("bridge_read_ranges", { chatMid: "c1" }), [{
    chatId: "c1",
    ranges: { u1: [{ startMessageId: "1", endMessageId: "10" }] },
  }]);
});
