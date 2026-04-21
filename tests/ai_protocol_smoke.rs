//! Integration smoke: `--features ai-protocol` must link `ai-lib-rust` (migration plan Phase 0).
//!
//! Run: `cargo test --features ai-protocol --test ai_protocol_smoke`

#![cfg(feature = "ai-protocol")]

use std::any::TypeId;

#[test]
fn ai_lib_rust_types_are_reachable() {
    // Compile-time proof that the optional dependency resolves.
    assert_eq!(
        TypeId::of::<ai_lib_rust::AiClient>(),
        TypeId::of::<ai_lib_rust::AiClient>()
    );
}
