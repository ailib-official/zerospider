//! Contract tests: keep CONTRIBUTING and ai-lib migration doc keywords from being dropped accidentally.
//!
//! Run: `cargo test --test docs_contract_contributing`

#[test]
fn contributing_mentions_protocol_env_and_checkout() {
    let s = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/CONTRIBUTING.md"));
    for needle in [
        "ailib-official/ai-protocol",
        "AI_PROTOCOL_DIR",
        "docs/ai-lib-migration.md",
        "cargo test --features ai-protocol",
    ] {
        assert!(
            s.contains(needle),
            "CONTRIBUTING.md should contain {needle:?}"
        );
    }
}

#[test]
fn ai_lib_migration_doc_mentions_protocol_env() {
    let s = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/ai-lib-migration.md"
    ));
    for needle in ["AI_PROTOCOL_DIR", "ai-lib-rust", "ai-protocol"] {
        assert!(
            s.contains(needle),
            "docs/ai-lib-migration.md should contain {needle:?}"
        );
    }
}

#[test]
fn migration_legacy_doc_contract() {
    let s = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/migration-legacy-to-protocol.md"
    ));
    for needle in [
        "AI_PROTOCOL_DIR",
        "legacy-providers",
        "ai-protocol",
        "ZS-ML-005",
    ] {
        assert!(
            s.contains(needle),
            "docs/migration-legacy-to-protocol.md should contain {needle:?}"
        );
    }
}
