/** Narrow protocol process: authentication, LINE RPC, events and call wire transport. */
import { Client, loginWithAuthToken, loginWithQR } from "@evex/linejs";
import { FileStorage } from "@evex/linejs/storage";
import { AuthStore, type LineDevice } from "./auth.ts";
import { createApi } from "./api.ts";
import { attachLive, errorReport } from "./live.ts";
import { packRaw } from "./raw_codec.ts";
import * as calls from "./calls.ts";
import { unsendFromOperation } from "./events.ts";
import { readFromOperation } from "./receipts.ts";

type Json = Record<string, unknown>;
const worker = globalThis as unknown as {
  onmessage: ((event: MessageEvent) => void) | null;
  postMessage: (data: unknown) => void;
};
const data: string = await new Promise((resolve) => {
  worker.onmessage = ({ data }) => resolve(data.data);
});
if (!data) throw new Error("protocol data directory argument required");
const queued: Json[] = [];
worker.onmessage = ({ data }) => queued.push(data);
const protocolData = `${data}/protocol-state`;
const transfer = `${data}/cache/transfer`;
const auth = new AuthStore(protocolData);
const storage = new FileStorage(`${protocolData}/linejs-storage.json`);
let client: Client | null = null;
let stopLive: (() => void) | null = null;
function emit(value: Json) {
  // Profiles and Thrift responses may contain bigint, which cannot be written
  // directly by the launcher's JSON serializer.
  worker.postMessage(packRaw(value));
}
function event(name: string, payload: Json = {}) {
  emit({ event: name, ...payload });
}
function ok(id: number | string | null, result: unknown = {}) {
  emit({ id, ok: true, result });
}
function fail(id: number | string | null, error: unknown) {
  emit({
    id,
    ok: false,
    error: error instanceof Error ? error.message : String(error),
  });
}
for (const level of ["log", "info", "debug", "warn"] as const) {
  console[level] = console.error.bind(console);
}
calls.configureCallRuntime({
  getClient: () => client,
  loadAuthDevice: () => auth.loadDevice(),
  emitEvent: event,
  ok,
  fail,
});
calls.setAudioSocket(`${protocolData}/audio.sock`);
const api = createApi(() => client, transfer);
const device =
  (Deno.env.get("LINE_DEVICE") || "ANDROIDSECONDARY") as LineDevice;
const version = Deno.env.get("LINE_VERSION") || "26.6.2";

async function connected(next: Client, loginDevice: LineDevice) {
  const profile = await next.getMyProfile();
  stopLive?.();
  client = next;
  await auth.save(next.authToken, loginDevice);
  event("core_session", { ...profile });
  const live = attachLive(next, profile.mid, () => client === next, event);
  stopLive = live.stop;
  next.on("event", (op) => {
    if (client !== next) return;
    try {
      const unsent = unsendFromOperation(op);
      const read = readFromOperation(op);
      if (unsent) event("message_unsent", unsent);
      else if (read) {
        event("peer_read", read);
      } else if (/CONTACT|FRIEND|PROFILE/.test(String(op.type))) {
        event("catalog_invalidated");
      }
    } catch (error) {
      event("protocol_error", errorReport(error, "live_event"));
    }
  });
  next.on("call:incoming", calls.handleIncomingCall);
  next.on("call:cancel", calls.handleCallCancel);
  live.start();
  return profile;
}

async function handle(request: Json) {
  const id = (request.id ?? null) as number | string | null;
  const p = (request.params ?? {}) as Json;
  const method = String(request.method);
  try {
    if (method.startsWith("bridge_")) {
      ok(id, await api(method, p));
      return;
    }
    switch (method) {
      case "ping":
        ok(id, { version: "rust-core-v2" });
        break;
      case "login_qr": {
        const next = await loginWithQR({
          onReceiveQRUrl: (url) => event("qr", { url }),
          onPincodeRequest: (pin) => event("pin", { pin }),
        }, { device, version, storage });
        ok(id, await connected(next, device));
        break;
      }
      case "login_token": {
        const token = String(p.token || await auth.loadToken() || "");
        ok(
          id,
          await connected(
            await loginWithAuthToken(token, { device, version, storage }),
            device,
          ),
        );
        break;
      }
      case "logout":
      case "end_session":
        client = null;
        try {
          stopLive?.();
        } finally {
          stopLive = null;
        }
        await auth.clear();
        await storage.clear();
        ok(id);
        break;
      case "call_start":
      case "call_answer":
        calls.setCallAudioDevices(
          p.audioInput as string,
          p.audioOutput as string,
        );
        calls.setCallGains(p.micGain, p.spkGain);
        if (method === "call_start") await calls.doCallStart(id, String(p.mid));
        else await calls.doCallAnswer(id);
        break;
      case "call_decline":
        await calls.doCallDecline(id);
        break;
      case "call_end":
        await calls.doCallEnd(id);
        break;
      case "call_set_audio":
        ok(id, calls.updateCallAudio(p));
        break;
      default:
        throw new Error(`unknown_method:${method}`);
    }
  } catch (error) {
    event("protocol_error", errorReport(error, "request"));
    fail(id, error);
  }
}
globalThis.addEventListener("unhandledrejection", (e) => {
  e.preventDefault();
  event("protocol_error", errorReport(e.reason, "unhandledrejection"));
});
const token = await auth.loadToken();
event("ready", { hasAuth: !!token });
if (token) {
  try {
    const saved = await auth.loadDevice();
    const profile = await connected(
      await loginWithAuthToken(token, { device: saved, version, storage }),
      saved,
    );
    event("session", { ...profile });
  } catch (error) {
    event("protocol_error", errorReport(error, "boot_restore"));
    event("session_failed", { error: String(error) });
  }
}
worker.onmessage = ({ data }) => {
  void handle(data);
};
for (const request of queued) void handle(request);
