/** Authenticated LINE Notes operations. Presentation stays in Rust. */
import type { Client } from "@evex/linejs";

type Json = Record<string, unknown>;
function object(value: unknown): Json {
  return value && typeof value === "object" && !Array.isArray(value)
    ? value as Json
    : {};
}
function identifier(value: unknown): string {
  if (typeof value !== "string" || !/^[a-zA-Z0-9_-]{1,200}$/.test(value)) {
    throw new Error("invalid_note_identifier");
  }
  return value;
}

export async function notesRpc(
  c: Client,
  action: string,
  p: Json,
  transfer?: string,
) {
  const chat = identifier(p.chatMid);
  if (!/^[ucr]/.test(chat)) throw new Error("notes_chat_unsupported");
  const timeline = c.base.timeline;
  await timeline.initTimeline();
  async function request(
    path: string,
    params: Record<string, string>,
    body?: unknown,
  ): Promise<unknown> {
    const response = await c.base.fetch(
      `https://${c.base.request.endpoint}/ext/note/nt/api/${path}.json?${new URLSearchParams(
        params,
      )}`,
      {
        method: "POST",
        headers: {
          ...timeline.timelineHeaders,
          "x-lhm": body === undefined ? "GET" : "POST",
        },
        body: body === undefined ? undefined : JSON.stringify(body),
      },
    );
    if (!response.ok) throw new Error(`notes_http_${response.status}`);
    const envelope = object(await response.json());
    if (envelope.code !== 0) {
      // Do not leak server payloads, channel tokens or authentication headers.
      throw new Error(
        `notes_api_${String(envelope.code ?? "invalid_response")}`,
      );
    }
    if (!("result" in envelope)) throw new Error("notes_invalid_response");
    if (envelope.result === false) throw new Error("notes_operation_rejected");
    return envelope.result;
  }

  // A peer MID is a public home, NOT the private one-to-one notebook.
  // Resolve OTO every request so state never leaks between signed-in accounts.
  let home = chat;
  if (chat.startsWith("u")) {
    const own = identifier(c.base.profile?.mid);
    const sync = object(await request("v57/otoaccount/sync", { userMid: own }));
    if (!Array.isArray(sync.updated)) {
      throw new Error("notes_invalid_oto_response");
    }
    const match = sync.updated.map(object).find((item) =>
      item.userMid === chat
    );
    if (match) {
      home = identifier(match.groupId);
    } else if (action === "create") {
      const created = object(
        await request("v57/otoaccount/create", {}, {
          userMid: own,
          friendMid: chat,
        }),
      );
      home = identifier(created.groupId);
    } else if (action === "list") {
      return { feeds: [] };
    } else {
      throw new Error("notes_not_found");
    }
    if (home.startsWith("u")) throw new Error("notes_invalid_oto_home");
  }
  const params = { homeId: home, sourceType: "TALKROOM" };
  const text = () => {
    if (typeof p.text !== "string" || !p.text.trim() || p.text.length > 10000) {
      throw new Error("notes_invalid_text");
    }
    return p.text;
  };
  if (action === "list") {
    const query: Record<string, string> = {
      ...params,
      likeLimit: "0",
      commentLimit: "0",
    };
    if (p.cursor != null) {
      const cursor = object(p.cursor);
      query.postId = identifier(cursor.postId);
      if (!/^\d+$/.test(String(cursor.updatedTime))) {
        throw new Error("notes_invalid_cursor");
      }
      query.updatedTime = String(cursor.updatedTime);
    }
    return await request("v57/post/list", query);
  }
  if (action === "create") {
    const content = typeof p.text === "string" ? p.text : "";
    const files = Array.isArray(p.files) ? p.files : [];
    if (
      (!content.trim() && files.length === 0) || content.length > 10000 ||
      files.length > 20
    ) {
      throw new Error("notes_invalid_text");
    }
    const media = [];
    for (const file of files) {
      const item = object(file);
      if (
        !transfer || typeof item.name !== "string" ||
        !/^[a-zA-Z0-9_-]+\.[a-zA-Z0-9]+$/.test(item.name)
      ) throw new Error("invalid_transfer_name");
      if (item.type !== "image" && item.type !== "video") {
        throw new Error("notes_invalid_media");
      }
      const bytes = await Deno.readFile(`${transfer}/${item.name}`);
      const uploaded = await timeline.uploadNoteMedia(
        item.type,
        new Blob([bytes]),
      );
      media.push({
        objectId: uploaded.objId,
        type: item.type === "image" ? "PHOTO" : "VIDEO",
        obsFace: "[]",
      });
    }
    return await request("v57/post/create", params, {
      postInfo: { readPermission: { type: "GROUP", gids: [home] } },
      contents: { text: content, stickers: [], locations: [], media },
    });
  }
  const postId = identifier(p.postId);
  if (action === "media") {
    if (!transfer) throw new Error("notes_transfer_missing");
    const result = object(await request("v57/post/get", { ...params, postId }));
    const contents = object(object(result.post ?? result).contents);
    const media = Array.isArray(contents.media)
      ? contents.media.map(object)
      : [];
    const item = media.find((entry) => entry.objectId === p.objectId);
    if (!item || !["PHOTO", "VIDEO"].includes(String(item.type))) {
      throw new Error("notes_invalid_media");
    }
    const oid = identifier(item.objectId);
    const headers = Object.fromEntries(
      Object.entries(timeline.timelineHeaders).filter((
        entry,
      ): entry is [string, string] => typeof entry[1] === "string"),
    );
    const blob = await c.base.obs.downloadObjectForService({
      obsPath: "myhome/h",
      oid,
      addHeaders: headers,
    });
    const path = `${transfer}/${crypto.randomUUID()}.part`;
    const file = await Deno.open(path, {
      createNew: true,
      write: true,
      mode: 0o600,
    });
    try {
      await blob.stream().pipeTo(file.writable);
    } catch (error) {
      await Deno.remove(path).catch(() => {});
      throw error;
    }
    return { path, type: item.type, mime: blob.type };
  }
  if (action === "get") {
    const post = await request("v57/post/get", { ...params, postId });
    // A comment permission/network failure must not hide the post itself.
    let comments: unknown = null;
    let commentsError = false;
    try {
      comments = await request("v57/comment/getList", {
        ...params,
        contentId: postId,
      });
    } catch {
      commentsError = true;
    }
    return { post, comments, commentsError };
  }
  if (action === "comments") {
    const query: Record<string, string> = { ...params, contentId: postId };
    if (typeof p.scrollId === "string" && p.scrollId.length <= 4096) {
      query.scrollId = p.scrollId;
    }
    return await request("v57/comment/getList", query);
  }
  if (action === "comment") {
    return await request("v57/comment/create", params, {
      contentId: postId,
      commentText: text(),
      contentsList: [],
    });
  }
  if (action === "like") {
    if (p.liked === false) {
      return await request("v41/like/cancel", { ...params, contentId: postId });
    }
    return await request("v57/like/create", params, {
      sourceType: "TALKROOM",
      contentId: postId,
      actorId: home,
      likeType: "1003",
      sharable: false,
    });
  }
  if (action === "update" || action === "delete") {
    const result = object(await request("v57/post/get", { ...params, postId }));
    const post = object(result.post ?? result);
    const writer = object(post.writerInfo);
    const info = object(post.postInfo);
    if ((writer.writerMid ?? writer.mid) !== c.base.profile?.mid) {
      throw new Error("notes_not_owner");
    }
    if (String(info.postId) !== postId) throw new Error("notes_invalid_post");
    if (action === "delete") {
      return await request("v57/post/delete", { ...params, postId });
    }
    // Patch the freshly fetched contents, retaining photos, video, stickers,
    // locations, styles and permissions instead of rebuilding a text-only post.
    if (!post.contents || typeof post.contents !== "object") {
      throw new Error("notes_invalid_post");
    }
    return await request("v57/post/update", params, {
      postInfo: { ...info, editableContents: ["ALL"] },
      contents: { ...object(post.contents), text: text(), textMeta: [] },
    });
  }
  throw new Error("notes_unknown_action");
}
