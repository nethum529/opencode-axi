use oca_git::RefId;

#[test]
fn ref_id_accepts_canonical_values() {
    for value in ["w00000", "w4f2a1", "wzzzzz"] {
        assert!(RefId::new(value).is_ok(), "{value} should be accepted");
    }
}

#[test]
fn ref_id_rejects_noncanonical_values() {
    for value in [
        "x4f2a1", "w4f2a", "w4f2a10", "w4F2a1", "w4f-a1", "w4f_a1", "wé0000",
    ] {
        assert!(RefId::new(value).is_err(), "{value} should be rejected");
    }
}
