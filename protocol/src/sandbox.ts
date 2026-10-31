/** Shared by production launcher and permission-boundary tests. */
export function permissions(
  protocolData: string,
  transfer: string,
) {
  return {
    read: [protocolData, transfer],
    write: [protocolData, transfer],
    net: true,
    env: [
      "Q_DEBUG",
      "NODE_DEBUG",
      "NO_COLOR",
      "LINE_DEVICE",
      "LINE_VERSION",
      "LINE_CALL_DEVNAME",
      "LINE_CALL_DEVICE_INFO",
      "LINE_CALL_OPUS_SIGNAL",
      "LINE_CALL_DEBUG",
      "LINE_GTK_AUDIO_INPUT",
      "LINE_GTK_AUDIO_OUTPUT",
    ],
    run: false,
    ffi: false,
    sys: false,
  };
}
