/** Compiled launcher starts the sandboxed protocol worker with host-selected paths. */
import { join } from "@std/path";
import { permissions } from "./sandbox.ts";
import { wireLine } from "./raw_codec.ts";
const data = Deno.args[0] || Deno.env.get("LINE_GTK_DATA");
if (!data) throw new Error("LINE_GTK_DATA or first argument required");
// These roots are application IPC/auth areas, never the message database or user files.
const protocolData = join(data, "protocol-state");
const transfer = join(data, "cache", "transfer");
await Deno.mkdir(protocolData, { recursive: true, mode: 0o700 });
await Deno.mkdir(transfer, { recursive: true, mode: 0o700 });
const worker = new Worker(new URL("./runtime.ts", import.meta.url).href, {
  type: "module",
  deno: {
    permissions: permissions(protocolData, transfer),
  },
});
const ready = Promise.withResolvers<void>();
worker.onerror = (e) => {
  console.error(e.message);
  ready.reject(new Error(e.message));
};
worker.onmessage = ({ data }) => {
  Deno.stdout.writeSync(new TextEncoder().encode(wireLine(data)));
  if (data.event === "ready") ready.resolve();
};
worker.postMessage({ data });
await ready.promise;
const decoder = new TextDecoder();
let pending = "";
for await (const chunk of Deno.stdin.readable) {
  pending += decoder.decode(chunk, { stream: true });
  let nl;
  while ((nl = pending.indexOf("\n")) >= 0) {
    const line = pending.slice(0, nl);
    pending = pending.slice(nl + 1);
    if (line.trim()) {
      try {
        worker.postMessage(JSON.parse(line));
      } catch (error) {
        console.error(error);
      }
    }
  }
}
worker.terminate();
