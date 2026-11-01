/** Wire exception reporting and per-message decode envelopes; Rust owns policy. */
import type { Client, TalkMessage } from "@evex/linejs";
import { packRaw } from "./raw_codec.ts";
type Json = Record<string, unknown>;
// History also calls the guarded decoder. Preserve its outcome across both paths.
const envelopes = new WeakMap<object, Json>();
export function takeDecodeEnvelope(raw: object): Json | undefined {
  const result = envelopes.get(raw);
  envelopes.delete(raw);
  return result;
}

export function errorReport(error: unknown, source: string): Json {
  const value = error as {
    code?: unknown;
    message?: unknown;
    data?: { code?: unknown };
  } | null;
  return {
    source,
    code: String(value?.data?.code ?? value?.code ?? ""),
    error: error instanceof Error
      ? error.message
      : typeof error === "string"
      ? error
      : String(value?.message ?? "protocol error"),
  };
}

export function attachLive(
  client: Client,
  account: string,
  active: () => boolean,
  emit: (event: string, value: Json) => void,
) {
  const controller = new AbortController();
  type Raw = Parameters<typeof client.base.e2ee.decryptE2EEMessage>[0];
  const original = client.base.e2ee.decryptE2EEMessage.bind(client.base.e2ee);
  client.base.e2ee.decryptE2EEMessage = async (raw: Raw) => {
    // Snapshot before a failing decoder can mutate the encrypted metadata.
    const packed = packRaw(raw);
    try {
      const decoded = await original(raw);
      envelopes.set(decoded, {
        decodedMessage: packRaw(decoded),
        rawMessage: packed,
        decodeError: null,
      });
      return decoded;
    } catch (error) {
      if (active()) emit("protocol_error", errorReport(error, "live_decode"));
      // Resolve this one operation so linejs' polling loop can process the next.
      // Rust receives the failure envelope rather than presenting raw ciphertext.
      const fallback = { ...raw };
      envelopes.set(fallback, {
        decodedMessage: null,
        rawMessage: packed,
        decodeError: "message_decode_failed",
      });
      return fallback;
    }
  };
  client.on("message", (message) => {
    if (!active()) return;
    try {
      const raw = (message as TalkMessage).raw;
      const to = String(raw.to ?? "");
      const chatMid = to.startsWith("c") || to.startsWith("r")
        ? to
        : raw.from === account
        ? to
        : String(raw.from);
      emit("message", {
        chatMid,
        message: takeDecodeEnvelope(raw) ??
          {
            decodedMessage: packRaw(raw),
            rawMessage: packRaw(raw),
            decodeError: null,
          },
      });
    } catch (error) {
      emit("protocol_error", errorReport(error, "live_event"));
    }
  });
  // linejs catches stream exceptions internally; forward its failure log too.
  client.base.on("log", (log) => {
    if (active() && log.type === "LegyPusherError") {
      emit("protocol_error", errorReport(log.data.error, "live_stream"));
    }
  });
  return {
    start: () =>
      client.listen({ talk: true, square: false, signal: controller.signal }),
    stop: () => {
      client.base.disabled = true;
      controller.abort();
    },
  };
}
