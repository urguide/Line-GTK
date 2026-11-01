use line_rust_core::attachments::{FALLBACK_DURATION_MS, duration_ms};

#[test]
fn explicit_duration_wins_without_running_probe() {
    assert_eq!(duration_ms(Some(3200.0), || panic!("unneeded probe")), 3200);
    assert_eq!(duration_ms(Some(1234.6), || panic!("unneeded probe")), 1235);
}

#[test]
fn probe_duration_is_rounded_and_invalid_values_use_legacy_fallback() {
    assert_eq!(duration_ms(None, || Some(2.3456)), 2346);
    assert_eq!(duration_ms(None, || Some(0.0001)), 1);
    for seconds in [
        None,
        Some(f64::NAN),
        Some(f64::INFINITY),
        Some(-1.0),
        Some(0.0),
        Some(1e30),
    ] {
        assert_eq!(duration_ms(None, || seconds), FALLBACK_DURATION_MS);
    }
    for explicit in [f64::NAN, f64::INFINITY, -1.0, 0.0, 1e30] {
        assert_eq!(duration_ms(Some(explicit), || Some(2.0)), 2000);
        assert_eq!(duration_ms(Some(explicit), || None), FALLBACK_DURATION_MS);
    }
}
