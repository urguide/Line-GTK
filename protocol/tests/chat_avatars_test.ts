import { assertEquals } from "@std/assert";
import { join } from "@std/path";
import { policyFor } from "../src/cache_policy.ts";
import { type CommandRuntime, createCommandService } from "../src/commands.ts";

Deno.test("chat listing forwards group avatars to background hydration", async () => {
  const dir = await Deno.makeTempDir();
  const hydrated: { mid: string; picturePath?: string | null }[] = [];
  const refreshes: (boolean | undefined)[] = [];
  let cache: ReturnType<CommandRuntime["getChatCache"]> = null;
  const runtime = {
    getClient: () => ({
      base: {
        talk: {
          getMessageBoxes: () =>
            Promise.resolve({ messageBoxes: [{ id: "cRecent" }] }),
        },
      },
      fetchJoinedChats: () =>
        Promise.resolve([
          {
            mid: "cRecent",
            name: "Recent group",
            raw: { picturePath: "/recent-picture" },
          },
          {
            mid: "cJoined",
            name: "Joined group",
            raw: { picturePath: "/joined-picture" },
          },
        ]),
    }),
    getChatCache: () => cache,
    setChatCache: (value) => {
      cache = value;
    },
    contactIndex: new Map(),
    avatarDir: dir,
    chatCachePath: join(dir, "chats.json"),
    refreshCachePolicy: () => Promise.resolve(policyFor("smart")),
    cachePolicy: () => policyFor("smart"),
    dedupe: (_key, fn) => fn(),
    storeBoxCursor: () => {},
    existingFile: () => Promise.resolve(null),
    loadDiskContacts: () => Promise.resolve(),
    refreshContactIndex: (force) => {
      refreshes.push(force);
      return Promise.resolve();
    },
    hydrateAvatars: (chats) => {
      hydrated.push(...chats);
      return Promise.resolve();
    },
    hydratePreviews: () => Promise.resolve(),
    emitEvent: () => {},
    ok: () => {},
  } satisfies Partial<Omit<CommandRuntime, "getClient">> & {
    getClient: () => unknown;
  };
  try {
    const service = createCommandService(runtime as unknown as CommandRuntime);
    await service.listChats(1, true);
    assertEquals(
      hydrated.map(({ mid, picturePath }) => ({ mid, picturePath })),
      [
        { mid: "cJoined", picturePath: "/joined-picture" },
        { mid: "cRecent", picturePath: "/recent-picture" },
      ],
    );
    assertEquals(refreshes, [true]);

    // A prior download may have failed; a warm list must retry hydration too.
    hydrated.length = 0;
    await service.listChats(2);
    assertEquals(hydrated.length, 2);
    assertEquals(refreshes, [true]);
  } finally {
    await Deno.remove(dir, { recursive: true });
  }
});
