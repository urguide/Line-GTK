/** Decode LINE's withdrawal operation; persistence is entirely Rust-owned. */
export function unsendFromOperation(
  op: { type?: string | number; param1?: string; param2?: string },
): { chatMid: string; messageId: string } | null {
  if (
    !["DESTROY_MESSAGE", "NOTIFIED_DESTROY_MESSAGE", "64", "65"].includes(
      String(op.type),
    )
  ) return null;
  const chatMid = op.param1 ?? "", messageId = op.param2 ?? "";
  return chatMid && messageId ? { chatMid, messageId } : null;
}
