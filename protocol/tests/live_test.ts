import { assertEquals } from "@std/assert";
import { Client } from "@evex/linejs";
import { attachLive, errorReport } from "../src/live.ts";
import { createApi } from "../src/api.ts";

Deno.test("live decode failure returns an envelope and subsequent messages still arrive", async () => {
  let handler: ((message: unknown) => void) | undefined;
  let logHandler: ((log: unknown) => void) | undefined;
  let listening = false;
  let active = true;
  let aborted = false;
  const client = {
    base: {
      disabled: false,
      e2ee: {
        decryptE2EEMessage: (raw: Record<string, unknown>) => {
          if (raw.id === "1") {
            raw.contentMetadata = { modified: true };
            throw new Error("decode failed");
          }
          return Promise.resolve({ ...raw, text: "readable" });
        },
      },
      on: (_name: string, cb: (log: unknown) => void) => {
        logHandler = cb;
      },
      push: { opStream: { close: () => {} }, sqStream: { close: () => {} } },
    },
    on: (_name: string, cb: (message: unknown) => void) => {
      handler = cb;
    },
    listen: (options: { signal: AbortSignal }) => {
      listening = true;
      options.signal.addEventListener("abort", () => {
        aborted = true;
      });
    },
  } as unknown as Client;
  const events: { name: string; value: Record<string, unknown> }[] = [];
  const live = attachLive(
    client,
    "me",
    () => active,
    (name, value) => events.push({ name, value }),
  );
  live.start();
  for (const id of ["1", "2"]) {
    const raw = await client.base.e2ee.decryptE2EEMessage(
      {
        id,
        from: "peer",
        to: "me",
        contentType: "NONE",
        contentMetadata: { e2eeVersion: "2" },
      } as never,
    );
    handler!({ raw });
  }
  assertEquals(listening, true);
  assertEquals(client.base.disabled, false);
  assertEquals(events.map((e) => e.name), [
    "protocol_error",
    "message",
    "message",
  ]);
  const failed = events[1].value.message as Record<string, unknown>;
  assertEquals(failed.decodeError, "message_decode_failed");
  assertEquals(failed.decodedMessage, null);
  assertEquals((failed.rawMessage as Record<string, unknown>).contentMetadata, {
    e2eeVersion: "2",
  });
  assertEquals(
    ((events[2].value.message as Record<string, unknown>)
      .decodedMessage as Record<string, unknown>).text,
    "readable",
  );
  logHandler!({
    type: "LegyPusherError",
    data: { error: new Error("NOT_AUTHORIZED") },
  });
  assertEquals(events.at(-1)?.name, "protocol_error");
  assertEquals(
    client.base.disabled,
    false,
    "Rust, not TS, decides when to end session",
  );
  active = false;
  live.stop();
  assertEquals(aborted, true);
  assertEquals(client.base.disabled, true);
  const count = events.length;
  handler!({ raw: { id: "3", to: "me", from: "peer" } });
  assertEquals(events.length, count);
});

Deno.test("protocol reports structured authentication code without deciding logout", () => {
  assertEquals(
    errorReport({
      data: { code: "NOT_AUTHORIZED_DEVICE" },
      message: "request failed",
    }, "live_stream"),
    {
      source: "live_stream",
      code: "NOT_AUTHORIZED_DEVICE",
      error: "request failed",
    },
  );
  assertEquals(
    errorReport(new Error("decode failed"), "live_decode").error,
    "decode failed",
  );
});

Deno.test("guarded live decoder does not turn failed history decryption into success", async () => {
  const client = {
    base: {
      e2ee: {
        decryptE2EEMessage: () => {
          throw new Error("bad key");
        },
      },
      talk: {
        getPreviousMessagesV2WithRequest: () =>
          Promise.resolve([{
            id: "1",
            deliveredTime: 1,
            from: "peer",
            to: "me",
            contentMetadata: { e2eeVersion: "2" },
          }]),
      },
      on: () => {},
    },
    on: () => {},
    listen: () => {},
  } as unknown as Client;
  attachLive(client, "me", () => true, () => {});
  const api = createApi(
    () => client,
    "/unused",
    async (
      raw,
    ) => ({ raw: await client.base.e2ee.decryptE2EEMessage(raw) } as never),
  );
  const page = await api("bridge_fetch_page", {
    chatMid: "peer",
    before: { messageId: "2", deliveredTime: "2" },
  }) as { messages: Record<string, unknown>[] };
  assertEquals(page.messages[0].decodedMessage, null);
  assertEquals(page.messages[0].decodeError, "message_decode_failed");
});

Deno.test("actual linejs polling continues after one failed decrypt", async () => {
  const finished = Promise.withResolvers<void>();
  const base = {
    e2ee: {
      decryptE2EEMessage: (raw: Record<string, unknown>) => {
        if (raw.id === "1") throw new Error("missing key");
        return Promise.resolve({ ...raw, text: "next message" });
      },
    },
    createPolling: () => ({
      async *listenTalkEvents() {
        try {
          for (const id of ["1", "2"]) {
            yield {
              type: "RECEIVE_MESSAGE",
              message: {
                id,
                from: "peer",
                to: "me",
                contentType: "NONE",
                contentMetadata: { e2eeVersion: "2" },
              },
            };
          }
        } finally {
          finished.resolve();
        }
      },
    }),
    on: () => {},
    log: () => {},
    push: { opStream: { close: () => {} }, sqStream: { close: () => {} } },
  };
  const client = new Client(
    base as unknown as ConstructorParameters<typeof Client>[0],
  );
  const messages: Record<string, unknown>[] = [];
  const live = attachLive(client, "me", () => true, (event, payload) => {
    if (event === "message") {
      messages.push(payload.message as Record<string, unknown>);
    }
  });
  live.start();
  await finished.promise;
  assertEquals(messages.length, 2);
  assertEquals(messages[0].decodeError, "message_decode_failed");
  assertEquals(
    (messages[1].decodedMessage as Record<string, unknown>).text,
    "next message",
  );
  live.stop();
});
