use line_gtk_core::attachments::{mime, upload_method};

#[test]
fn rooms_and_other_legacy_destinations_use_obs_without_downgrading_e2ee_chats() {
    for mid in ["u-person", "c-group"] {
        assert_eq!(upload_method(mid).unwrap(), "bridge_send_media");
    }
    for mid in ["r-room", "m-square", "t-square-chat"] {
        assert_eq!(upload_method(mid).unwrap(), "bridge_upload_plain_media");
    }
    assert!(upload_method("").is_err());
    assert_eq!(mime("gif", "gif"), "image/gif");
    assert_eq!(mime("m4a", "audio"), "audio/mp4");
}
