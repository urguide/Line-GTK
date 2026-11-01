use line_rust_core::attachments::file_extension;

#[test]
fn document_suffixes_preserve_preview_types_without_using_sender_paths() {
    for (name, expected) in [
        ("報告.PDF", "pdf"),
        ("notes.TXT", "txt"),
        ("readme.md", "md"),
        ("data.json", "json"),
        ("archive.tar.gz", "gz"),
        ("../../report.pdf", "pdf"),
        (r"C:\fake\notes.txt", "txt"),
        ("file.pdf/../../escape", "bin"),
        (".hidden", "bin"),
        ("name.", "bin"),
        ("name.pdf\0", "bin"),
        ("name.verylongextension", "bin"),
        ("name.p df", "bin"),
        ("name.pdf:stream", "bin"),
        ("extensionless", "bin"),
    ] {
        let ext = file_extension(Some(name), "");
        assert_eq!(ext, expected, "{name:?}");
        assert!(ext.bytes().all(|b| b.is_ascii_alphanumeric()));
    }
    assert_eq!(file_extension(None, "application/pdf"), "pdf");
    assert_eq!(
        file_extension(Some("notes"), "text/plain; charset=utf-8"),
        "txt"
    );
    assert_eq!(
        file_extension(Some("notes.txt"), "application/octet-stream"),
        "txt"
    );
}
