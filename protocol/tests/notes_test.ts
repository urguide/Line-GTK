import { assertEquals, assertRejects } from "@std/assert";
import type { Client } from "@evex/linejs";
import { notesRpc } from "../src/notes.ts";

function fake(responses: unknown[], account = "ume") {
  const calls: {
    path: string;
    query: Record<string, string>;
    body: unknown;
  }[] = [];
  const c = {
    base: {
      profile: { mid: account },
      request: { endpoint: "example.invalid" },
      timeline: { initTimeline: () => Promise.resolve(), timelineHeaders: {} },
      obs: {},
      fetch: (url: string, init: RequestInit) => {
        const parsed = new URL(url);
        calls.push({
          path: parsed.pathname,
          query: Object.fromEntries(parsed.searchParams),
          body: init.body ? JSON.parse(String(init.body)) : null,
        });
        const result = responses.shift();
        if (result instanceof Error) throw result;
        return Promise.resolve(Response.json(result));
      },
    },
  } as unknown as Client;
  return { c, calls };
}
const ok = (result: unknown) => ({ code: 0, result });

Deno.test("notes resolves private OTO instead of reading the peer public home", async () => {
  const { c, calls } = fake([
    ok({ updated: [{ userMid: "upeer", groupId: "gprivate" }] }),
    ok({ feeds: [] }),
  ]);
  await notesRpc(c, "list", {
    chatMid: "upeer",
    cursor: { postId: "9007199254740993", updatedTime: 123 },
  });
  assertEquals(calls[0].query.userMid, "ume");
  assertEquals(calls[1].query, {
    homeId: "gprivate",
    sourceType: "TALKROOM",
    likeLimit: "0",
    commentLimit: "0",
    postId: "9007199254740993",
    updatedTime: "123",
  });
});

Deno.test("opening an absent OTO notebook is read-only; creating resolves the private destination", async () => {
  const empty = fake([ok({ updated: [] })]);
  assertEquals(await notesRpc(empty.c, "list", { chatMid: "upeer" }), {
    feeds: [],
  });
  assertEquals(empty.calls.length, 1);
  const create = fake([
    ok({ updated: [] }),
    ok({ groupId: "gprivate" }),
    ok({}),
  ]);
  await notesRpc(create.c, "create", { chatMid: "upeer", text: "hello" });
  assertEquals(create.calls[1].body, { userMid: "ume", friendMid: "upeer" });
  assertEquals(create.calls[2].query.homeId, "gprivate");
  assertEquals((create.calls[2].body as { postInfo: unknown }).postInfo, {
    readPermission: { type: "GROUP", gids: ["gprivate"] },
  });
});

Deno.test("editing preserves server attachments and permissions and rejects other authors", async () => {
  const post = {
    postInfo: { postId: "123", readPermission: { type: "GROUP" } },
    writerInfo: { writerMid: "ume" },
    contents: {
      text: "old",
      media: [{ objectId: "photo" }],
      stickers: [{ id: "1" }],
      locations: [{ name: "place" }],
    },
  };
  const own = fake([ok({ post }), ok({})]);
  await notesRpc(own.c, "update", {
    chatMid: "cgroup",
    postId: "123",
    text: "new",
  });
  assertEquals((own.calls[1].body as { contents: unknown }).contents, {
    ...post.contents,
    text: "new",
    textMeta: [],
  });
  const other = fake([ok({ post })], "uother");
  await assertRejects(
    () => notesRpc(other.c, "delete", { chatMid: "cgroup", postId: "123" }),
    Error,
    "notes_not_owner",
  );
  assertEquals(other.calls.length, 1);
});

Deno.test("notes API errors never become empty lists or successful writes and are not retried", async () => {
  for (const action of ["list", "create", "like", "comment"]) {
    const { c, calls } = fake([{ code: 403, message: "secret response" }]);
    await assertRejects(
      () =>
        notesRpc(c, action, {
          chatMid: "cgroup",
          text: "hello",
          postId: "123",
        }),
      Error,
      "notes_api_403",
    );
    assertEquals(calls.length, 1);
  }
  const invalid = fake([]);
  await assertRejects(
    () => notesRpc(invalid.c, "list", { chatMid: "../x" }),
    Error,
    "invalid_note_identifier",
  );
  assertEquals(invalid.calls.length, 0);
});

Deno.test("a failed comments request still returns the note and reports the partial failure", async () => {
  const post = { postInfo: { postId: "123" } };
  const { c } = fake([ok(post), { code: 403 }]);
  assertEquals(await notesRpc(c, "get", { chatMid: "cgroup", postId: "123" }), {
    post,
    comments: null,
    commentsError: true,
  });
});

Deno.test("note uploads preserve staged originals and attach returned object IDs", async () => {
  const root = await Deno.makeTempDir();
  try {
    await Deno.writeFile(`${root}/note-1.png`, new Uint8Array([1, 2, 3]));
    const { c, calls } = fake([ok({})]);
    let uploads = 0;
    c.base.timeline.uploadNoteMedia = async (kind, data) => {
      assertEquals(kind, "image");
      assertEquals(
        new Uint8Array(await data.arrayBuffer()),
        new Uint8Array([1, 2, 3]),
      );
      uploads++;
      return { objId: "photo123", objHash: "hash" };
    };
    await notesRpc(c, "create", {
      chatMid: "cgroup",
      text: "",
      files: [{ name: "note-1.png", type: "image" }],
    }, root);
    assertEquals(uploads, 1);
    assertEquals(
      (calls[0].body as { contents: { media: unknown } }).contents.media,
      [{ objectId: "photo123", type: "PHOTO", obsFace: "[]" }],
    );
    await assertRejects(
      () =>
        notesRpc(c, "create", {
          chatMid: "cgroup",
          files: [{ name: "../private", type: "image" }],
        }, root),
      Error,
      "invalid_transfer_name",
    );
    assertEquals(uploads, 1);
  } finally {
    await Deno.remove(root, { recursive: true });
  }
});

Deno.test("note download validates membership in the requested post before fetching OBS", async () => {
  const root = await Deno.makeTempDir();
  try {
    const post = {
      post: { contents: { media: [{ objectId: "photo123", type: "PHOTO" }] } },
    };
    const { c } = fake([ok(post), ok(post)]);
    let downloads = 0;
    c.base.obs.downloadObjectForService = (
      p: { obsPath: string; oid: string },
    ) => {
      assertEquals(p.obsPath, "myhome/h");
      assertEquals(p.oid, "photo123");
      downloads++;
      return Promise.resolve(
        new Blob([new Uint8Array([4, 5, 6])], { type: "image/png" }),
      );
    };
    const result = await notesRpc(c, "media", {
      chatMid: "cgroup",
      postId: "123",
      objectId: "photo123",
    }, root) as { path: string; type: string };
    assertEquals(result.type, "PHOTO");
    assertEquals(await Deno.readFile(result.path), new Uint8Array([4, 5, 6]));
    await assertRejects(
      () =>
        notesRpc(c, "media", {
          chatMid: "cgroup",
          postId: "123",
          objectId: "other",
        }, root),
      Error,
      "notes_invalid_media",
    );
    assertEquals(downloads, 1);
  } finally {
    await Deno.remove(root, { recursive: true });
  }
});
