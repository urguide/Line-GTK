/** Profile RPC/OBS adapter. Rust owns image preparation and local caching. */
import type { Client } from "@evex/linejs";
import { Buffer } from "node:buffer";

type Json = Record<string, unknown>;
function object(value: unknown): Json {
  return value && typeof value === "object" && !Array.isArray(value)
    ? value as Json
    : {};
}
function identifier(value: unknown): string {
  if (typeof value !== "string" || !/^[a-zA-Z0-9_-]{1,200}$/.test(value)) {
    throw new Error("invalid_profile_identifier");
  }
  return value;
}

export async function profileRpc(c: Client, p: Json, transfer: string) {
  const mid = identifier(p.mid);
  const own = mid === c.base.profile?.mid;
  const action = p.action;
  if (action === "get") {
    const profile = own
      ? await c.getMyProfile()
      : await c.base.talk.getContact({ mid });
    let isFriend: boolean | null = null;
    if (!own) {
      try {
        isFriend = (await c.base.talk.getAllContactIds({})).includes(mid);
      } catch {
        /* Unknown friendship must not hide an otherwise usable profile. */
      }
    }
    return { profile, isFriend };
  }
  if (action === "add") {
    if (own) throw new Error("cannot_add_self");
    await c.base.relation.addFriendByMid({ mid });
    return { mid, added: true };
  }
  if (action !== "cover" && action !== "avatar" && action !== "background") {
    throw new Error("invalid_profile_action");
  }
  if (action !== "cover" && !own) throw new Error("cannot_edit_other_profile");

  const timeline = c.base.timeline;
  async function coverRequest(body?: Json): Promise<Json> {
    await timeline.initTimeline();
    const response = await c.base.fetch(
      `https://${c.base.request.endpoint}/hm/api/v1/home/cover.json?${new URLSearchParams(
        { homeId: mid },
      )}`,
      {
        method: "POST",
        headers: {
          ...timeline.timelineHeaders,
          "x-lhm": body ? "POST" : "GET",
        },
        body: body ? JSON.stringify(body) : undefined,
      },
    );
    if (!response.ok) throw new Error(`profile_cover_http_${response.status}`);
    const envelope = object(await response.json());
    if (
      envelope.code !== 0 || !("result" in envelope) ||
      envelope.result === false
    ) {
      throw new Error(
        `profile_cover_api_${String(envelope.code ?? "invalid")}`,
      );
    }
    return object(envelope.result);
  }

  if (action === "cover") {
    const cover = await coverRequest();
    const info = object(cover.coverObsInfo ?? cover.cover);
    const oid = cover.coverObjectId ?? info.objectId ?? info.coverObjectId;
    if (oid == null || oid === "") return { mid, path: null };
    const id = identifier(oid);
    const headers: Record<string, string> = {};
    for (const [key, value] of Object.entries(timeline.timelineHeaders)) {
      if (value !== undefined) headers[key] = value;
    }
    const service = identifier(info.serviceName ?? "myhome");
    const namespace = identifier(info.obsNamespace ?? "c");
    const blob = await c.base.obs.downloadObjectForService({
      obsPath: `${service}/${namespace}`,
      oid: id,
      addHeaders: headers,
    });
    if (!blob.size || blob.size > 20 * 1024 * 1024) {
      throw new Error("invalid_profile_image_size");
    }
    const path = `${transfer}/profile-cover-${crypto.randomUUID()}.jpg`;
    await Deno.writeFile(path, new Uint8Array(await blob.arrayBuffer()));
    return { mid, path };
  }

  const name = identifier(p.transferName);
  const data = await Deno.readFile(`${transfer}/${name}`);
  if (!data.length || data.length > 20 * 1024 * 1024) {
    throw new Error("invalid_profile_image_size");
  }
  const oid = action === "avatar"
    ? mid
    : crypto.randomUUID().replaceAll("-", "");
  if (action === "background") await timeline.initTimeline();
  // Check the HTTP status ourselves: linejs's generic OBS uploader does not.
  const response = await c.base.fetch(
    `https://obs.line-apps.com/r/${
      action === "avatar" ? "talk/p" : "myhome/c"
    }/${oid}`,
    {
      method: "POST",
      headers: {
        ...c.base.request.getHeader("POST"),
        ...(action === "background" ? timeline.timelineHeaders : {}),
        "content-type": "image/jpeg",
        "X-Obs-Params": Buffer.from(
          JSON.stringify({
            type: "image",
            ver: "2.0",
            name: "profile.jpg",
            oid,
          }),
        ).toString("base64"),
        "X-Talk-Meta": Buffer.from(
          JSON.stringify({ profileContext: { storyShare: false } }),
        ).toString("base64"),
      },
      body: new Blob([data], { type: "image/jpeg" }),
    },
  );
  if (!response.ok) throw new Error(`profile_upload_http_${response.status}`);
  const uploaded = response.headers.get("x-obs-oid");
  if (!uploaded) throw new Error("profile_upload_missing_object");
  if (action === "background") {
    await coverRequest({
      homeId: mid,
      coverObjectId: uploaded,
      storyShare: false,
      meta: {},
    });
  }
  return { mid, updated: true };
}
