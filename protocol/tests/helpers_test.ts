import { assertEquals } from "@std/assert";
import { friendName, picturePathOf, profileUrl } from "../src/contacts.ts";
import { coerceI64, isMediaType, normalizeRaw } from "../src/messages.ts";

Deno.test("message normalization supplies metadata", () => {
  assertEquals(normalizeRaw({ text: "hello" }), {
    text: "hello",
    contentMetadata: {},
  });
  assertEquals(isMediaType("IMAGE"), true);
  assertEquals(isMediaType("NONE"), false);
});

Deno.test("thrift integer coercion preserves large identifiers", () => {
  assertEquals(coerceI64("42"), 42);
  assertEquals(coerceI64("9007199254740993"), 9007199254740993n);
});

Deno.test("contact picture helpers normalize LINE paths", () => {
  assertEquals(picturePathOf({ pictureStatus: "/abc" }), "/abc");
  assertEquals(profileUrl("/abc"), "https://profile.line-scdn.net/abc");
  assertEquals(
    profileUrl("https://example.test/photo"),
    "https://example.test/photo",
  );
});

Deno.test("avatar paths cover groups and both contact API shapes", () => {
  for (
    const profile of [
      { raw: { chatName: "Group", picturePath: " /group " } },
      { picturePath: " ", pictureStatus: "/group" },
      { thumbnailUrl: "https://example.test/group" },
      { raw: { contact: { pictureStatus: "/group" } } },
      { raw: { targetProfileDetail: { picturePath: "/group" } } },
      { targetProfileDetail: { pictureStatus: "/group" } },
    ]
  ) {
    assertEquals(
      profileUrl(picturePathOf(profile)),
      "thumbnailUrl" in profile
        ? "https://example.test/group"
        : "https://profile.line-scdn.net/group",
    );
  }
  assertEquals(picturePathOf({ raw: null }), null);
  assertEquals(picturePathOf({ picturePath: "", pictureStatus: null }), null);
  assertEquals(
    picturePathOf({ picturePath: "/primary", pictureStatus: "/fallback" }),
    "/primary",
  );
});

Deno.test("friend fallback reads the current linejs profile shape", () => {
  assertEquals(
    friendName({
      mid: "u1",
      raw: { targetProfileDetail: { profileName: "Friend" } },
    }),
    "Friend",
  );
});
