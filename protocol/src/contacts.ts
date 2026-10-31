export function friendName(user: { mid: string; raw: unknown }): string {
  const raw = user.raw as {
    contact?: { displayName?: string };
    targetProfileDetail?: { profileName?: string };
  } | null;
  return raw?.targetProfileDetail?.profileName || raw?.contact?.displayName ||
    user.mid;
}

export function profileUrl(picturePath?: string | null): string | null {
  if (!picturePath) return null;
  if (picturePath.startsWith("http")) return picturePath;
  return `https://profile.line-scdn.net${
    picturePath.startsWith("/") ? "" : "/"
  }${picturePath}`;
}

export function picturePathOf(profile: unknown): string | null {
  const record = (value: unknown): Record<string, unknown> =>
    value && typeof value === "object" ? value as Record<string, unknown> : {};
  const root = record(profile);
  const raw = record(root.raw);
  // linejs wraps group data in raw; User.raw uses targetProfileDetail in v3.
  const sources = [
    root,
    record(root.targetProfileDetail),
    raw,
    record(raw.targetProfileDetail),
    record(raw.contact),
  ];
  for (const source of sources) {
    for (
      const candidate of [
        source.picturePath,
        source.pictureStatus,
        record(source.picture).path,
        source.thumbnailUrl,
      ]
    ) {
      if (typeof candidate === "string" && candidate.trim()) {
        return candidate.trim();
      }
    }
  }
  return null;
}
