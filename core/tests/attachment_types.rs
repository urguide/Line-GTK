use line_gtk_core::attachments::{mime, normalize_media_type};

#[test]
fn all_legacy_extensions_keep_their_upload_type_and_mime() {
    for (ext, kind, expected_mime) in [
        ("png", "image", "image/png"),
        ("jpg", "image", "image/jpeg"),
        ("jpeg", "image", "image/jpeg"),
        ("webp", "image", "image/webp"),
        ("bmp", "image", "image/bmp"),
        ("heic", "image", "image/jpeg"),
        ("gif", "gif", "image/gif"),
        ("mp4", "video", "video/mp4"),
        ("webm", "video", "video/webm"),
        ("mov", "video", "video/quicktime"),
        ("mkv", "video", "video/x-matroska"),
        ("m4v", "video", "video/mp4"),
        ("m4a", "audio", "audio/mp4"),
        ("mp3", "audio", "audio/mpeg"),
        ("wav", "audio", "audio/wav"),
        ("ogg", "audio", "audio/ogg"),
        ("oga", "audio", "audio/ogg"),
        ("aac", "audio", "audio/mp4"),
        ("flac", "audio", "audio/mp4"),
        ("pdf", "file", "application/octet-stream"),
    ] {
        for ext in [ext.to_owned(), ext.to_ascii_uppercase()] {
            for requested in ["auto", " AUTO ", "", "unsupported"] {
                let actual = normalize_media_type(requested, &format!("附件.{ext}"));
                assert_eq!(actual, kind, "{requested}/{ext}");
                assert_eq!(mime(&ext, actual), expected_mime);
            }
        }
    }
}

#[test]
fn explicit_types_take_precedence_except_images_that_are_gifs() {
    for (requested, filename, expected) in [
        ("image", "animation.gif", "gif"),
        (" IMAGE ", "ANIMATION.GIF", "gif"),
        ("file", "animation.gif", "file"),
        (" FILE ", "picture.bmp", "file"),
        ("video", "sound.flac", "video"),
        (" AUDIO ", "picture.heic", "audio"),
        ("GIF", "blob.bin", "gif"),
        ("image", "blob.bin", "image"),
        ("auto", "extensionless", "file"),
        ("image", "noextension", "image"),
        ("auto", "gif", "file"),
        ("auto", "picture.heic.txt", "file"),
    ] {
        assert_eq!(normalize_media_type(requested, filename), expected);
    }
    assert_eq!(mime("bin", "image"), "image/jpeg");
    assert_eq!(mime("bin", "video"), "video/mp4");
    assert_eq!(mime("bin", "audio"), "audio/mp4");
    // An explicit file type changes the LINE bubble type, not the file's MIME.
    assert_eq!(mime("GIF", "file"), "image/gif");
}
