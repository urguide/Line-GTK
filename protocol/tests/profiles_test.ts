import { assertEquals, assertRejects } from "@std/assert";
import type { Client } from "@evex/linejs";
import { profileRpc } from "../src/profiles.ts";

function fake(responses: Response[] = []) {
  const calls: { url: string; body: unknown }[] = [];
  const client = {
    base: {
      profile: { mid: "ume" },
      request: { endpoint: "example.invalid", getHeader: () => ({}) },
      timeline: { initTimeline: () => Promise.resolve(), timelineHeaders: {} },
      fetch: (url: string, init: RequestInit) => {
        calls.push({
          url,
          body: typeof init.body === "string"
            ? JSON.parse(init.body)
            : init.body,
        });
        return Promise.resolve(responses.shift()!);
      },
    },
  } as unknown as Client;
  return { client, calls };
}

Deno.test("profile cover binds the acknowledged OBS object only after upload succeeds", async () => {
  const root = await Deno.makeTempDir();
  try {
    await Deno.writeFile(`${root}/photo`, new Uint8Array([1, 2, 3]));
    const { client, calls } = fake([
      new Response(null, {
        status: 201,
        headers: { "x-obs-oid": "serverObject" },
      }),
      Response.json({ code: 0, result: true }),
    ]);
    assertEquals(
      await profileRpc(client, {
        mid: "ume",
        action: "background",
        transferName: "photo",
      }, root),
      { mid: "ume", updated: true },
    );
    assertEquals(
      new URL(calls[0].url).pathname.startsWith("/r/myhome/c/"),
      true,
    );
    assertEquals(calls[1].body, {
      homeId: "ume",
      coverObjectId: "serverObject",
      storyShare: false,
      meta: {},
    });
    for (
      const failure of [
        new Response(null, { status: 403 }),
        new Response(null, { status: 201 }),
      ]
    ) {
      const f = fake([failure]);
      await assertRejects(() =>
        profileRpc(f.client, {
          mid: "ume",
          action: "background",
          transferName: "photo",
        }, root)
      );
      assertEquals(f.calls.length, 1);
    }
    const rejected = fake([
      new Response(null, {
        status: 201,
        headers: { "x-obs-oid": "serverObject" },
      }),
      Response.json({ code: 403, result: false }),
    ]);
    await assertRejects(
      () =>
        profileRpc(rejected.client, {
          mid: "ume",
          action: "background",
          transferName: "photo",
        }, root),
      Error,
      "profile_cover_api_403",
    );
  } finally {
    await Deno.remove(root, { recursive: true });
  }
});

Deno.test("profile rejects editing another account and transfer path traversal before upload", async () => {
  const { client, calls } = fake();
  for (const action of ["avatar", "background"]) {
    await assertRejects(
      () =>
        profileRpc(
          client,
          { mid: "uother", action, transferName: "photo" },
          "/unused",
        ),
      Error,
      "cannot_edit_other_profile",
    );
    await assertRejects(
      () =>
        profileRpc(
          client,
          { mid: "ume", action, transferName: "../photo" },
          "/unused",
        ),
      Error,
      "invalid_profile_identifier",
    );
  }
  assertEquals(calls.length, 0);
});

Deno.test("cover reads coverObsInfo and distinguishes missing covers from API failure", async () => {
  const root = await Deno.makeTempDir();
  try {
    const { client } = fake([
      Response.json({
        code: 0,
        result: {
          coverObsInfo: {
            serviceName: "myhome",
            obsNamespace: "c",
            objectId: "cover1",
          },
        },
      }),
    ]);
    const downloads: unknown[] = [];
    Object.assign(client.base, {
      obs: {
        downloadObjectForService: (p: unknown) => {
          downloads.push(p);
          return Promise.resolve(new Blob(["photo"]));
        },
      },
    });
    const result = await profileRpc(
      client,
      { mid: "upeer", action: "cover" },
      root,
    );
    assertEquals(downloads, [{
      obsPath: "myhome/c",
      oid: "cover1",
      addHeaders: {},
    }]);
    assertEquals(await Deno.readTextFile(String(result.path)), "photo");
    const absent = fake([Response.json({ code: 0, result: {} })]);
    assertEquals(
      await profileRpc(absent.client, { mid: "upeer", action: "cover" }, root),
      { mid: "upeer", path: null },
    );
    const denied = fake([Response.json({ code: 403 })]);
    await assertRejects(
      () => profileRpc(denied.client, { mid: "upeer", action: "cover" }, root),
      Error,
      "profile_cover_api_403",
    );
  } finally {
    await Deno.remove(root, { recursive: true });
  }
});

Deno.test("contact load survives friendship lookup failure and add errors are not reported as success", async () => {
  const { client } = fake();
  Object.assign(client.base, {
    talk: {
      getContact: () =>
        Promise.resolve({
          mid: "upeer",
          displayName: "Peer",
          statusMessage: "Hello",
        }),
      getAllContactIds: () => Promise.reject(new Error("offline")),
    },
  });
  assertEquals<unknown>(
    await profileRpc(client, { mid: "upeer", action: "get" }, "/unused"),
    {
      profile: { mid: "upeer", displayName: "Peer", statusMessage: "Hello" },
      isFriend: null,
    },
  );
  Object.assign(client.base, {
    relation: {
      addFriendByMid: () => Promise.reject(new Error("denied")),
    },
  });
  await assertRejects(
    () => profileRpc(client, { mid: "upeer", action: "add" }, "/unused"),
    Error,
    "denied",
  );
  await assertRejects(
    () => profileRpc(client, { mid: "ume", action: "add" }, "/unused"),
    Error,
    "cannot_add_self",
  );
});
