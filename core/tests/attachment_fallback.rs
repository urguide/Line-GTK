use anyhow::bail;
use line_rust_core::attachments::upload;
use serde_json::json;

#[test]
fn successful_primary_and_plain_uploads_are_never_retried() {
    for (chat, expected) in [
        ("u1", "bridge_send_media"),
        ("c1", "bridge_send_media"),
        ("r1", "bridge_upload_plain_media"),
    ] {
        let mut calls = Vec::new();
        let result = upload(
            &json!({"chatMid":chat}),
            |method, _| {
                calls.push(method.to_owned());
                Ok(json!({"sent":true}))
            },
            || panic!("unexpected retry"),
        )
        .unwrap();
        assert_eq!(result["sent"], true);
        assert_eq!(calls, vec![expected]);
    }
    assert!(
        upload(
            &json!({"chatMid":"r1"}),
            |_, _| bail!("failed"),
            || panic!("plain retry")
        )
        .is_err()
    );
}

#[test]
fn fallback_is_once_encrypted_and_preserves_all_parameters() {
    for chat in ["u1", "c1"] {
        for both_fail in [false, true] {
            let params = json!({"chatMid":chat,"transferName":"upload.mp4","previewName":"preview.jpg","durationMs":3200,"fileName":"影片.mp4","oType":"video"});
            let mut methods = Vec::new();
            let mut retries = 0;
            let result = upload(
                &params,
                |method, p| {
                    assert_eq!(p, params);
                    methods.push(method.to_owned());
                    if method == "bridge_send_media" {
                        bail!("primary failed");
                    }
                    if both_fail {
                        bail!("fallback failed");
                    }
                    Ok(json!({"rawMessage":{"id":"123"}}))
                },
                || {
                    retries += 1;
                    Ok(())
                },
            );
            assert_eq!(
                methods,
                vec!["bridge_send_media", "bridge_send_media_e2ee_fallback"]
            );
            assert_eq!(retries, 1);
            assert_eq!(result.is_err(), both_fail);
        }
    }
}

#[test]
fn cancelled_or_unknown_outcome_does_not_resend() {
    for error in [
        "session_changed",
        "protocol timeout",
        "protocol disconnected",
        "bridge_busy",
        "not_logged_in",
    ] {
        let result = upload(
            &json!({"chatMid":"u1"}),
            |_, _| bail!("{error}"),
            || panic!("unsafe retry"),
        );
        assert_eq!(result.unwrap_err().to_string(), error);
    }
    let mut count = 0;
    assert!(
        upload(
            &json!({"chatMid":"u1"}),
            |_, _| {
                count += 1;
                bail!("primary failed")
            },
            || bail!("session_changed")
        )
        .is_err()
    );
    assert_eq!(count, 1);
}
