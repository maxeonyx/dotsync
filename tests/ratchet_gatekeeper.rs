// `cargo test` runs the suite without the ratchet's history checks, so every
// test binary would silently pass outside the workflow. This one refuses.

#[test]
fn tdd_ratchet_gatekeeper() {
    if std::env::var("TDD_RATCHET").is_err() {
        panic!("Run tdd-ratchet instead of cargo test.");
    }
}
