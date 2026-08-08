//! Property-based tests for the secret gate. The gate is the one piece
//! of wukong whose failure is a silent data leak, so its invariants are
//! worth checking against thousands of generated inputs rather than a
//! handful of hand-written lines.
//!
//! The invariants under test:
//! 1. `scan` and the mask functions never panic — on any input, however
//!    adversarial (multibyte, control chars, CRLF, gigantic lines).
//! 2. Masking every finding produces content that rescans CLEAN — the
//!    exact guarantee the redact-and-verify path leans on.
//! 3. A real secret, once masked, is gone: its full text never survives
//!    into the masked output.
//! 4. Fingerprints are deterministic and well-formed.

use proptest::prelude::*;
use std::path::Path;
use wukong_core::gate::{self, Finding, GateVerdict};

fn scan(content: &str) -> GateVerdict {
    gate::scan(Path::new(".zshrc"), content)
}

fn findings(content: &str) -> Vec<Finding> {
    match scan(content) {
        GateVerdict::Quarantine(f) => f,
        _ => Vec::new(),
    }
}

/// Tokens shaped like real credentials, so the "a secret is detected
/// and then removed" properties exercise the detectors rather than
/// trivially passing on inert text.
fn secret_token() -> impl Strategy<Value = String> {
    prop_oneof![
        // GitHub PAT.
        "ghp_[A-Za-z0-9]{36}",
        // AWS access key id.
        "AKIA[0-9A-Z]{16}",
        // Anthropic key.
        "sk-ant-[A-Za-z0-9]{24}",
        // 64-char hex (the shape a fixed entropy bar used to miss).
        "[0-9a-f]{64}",
        // base64-ish blob including the slash the old rule excluded.
        "[A-Za-z0-9+/]{40}",
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 400, ..ProptestConfig::default() })]

    /// Invariant 1: scanning arbitrary text never panics, and neither
    /// does masking whatever it finds.
    #[test]
    fn scan_and_mask_never_panic(content in any::<String>()) {
        let verdict = scan(&content);
        if let GateVerdict::Quarantine(fs) = verdict {
            let _ = gate::mask_findings(&content, &fs, |_| false);
            let _ = gate::mask_findings(&content, &fs, |_| true);
        }
        let _ = gate::mask_all(&content);
    }

    /// Invariant 1, sharpened: multibyte characters around and inside
    /// candidate spans must not trip a byte-vs-char slice panic.
    #[test]
    fn unicode_never_panics(content in "(?s)[\\x{00}-\\x{10FFFF}]{0,200}") {
        let _ = gate::mask_all(&content);
        if let GateVerdict::Quarantine(fs) = scan(&content) {
            prop_assert!(gate::mask_findings(&content, &fs, |_| false).is_char_boundary(0));
        }
    }

    /// Invariant 2: masking every finding yields content the gate
    /// considers clean. This is what lets the redact path trust its
    /// stored copy after a verify pass.
    #[test]
    fn masking_all_findings_rescans_clean(
        prefix in "[a-zA-Z_ ]{0,20}",
        token in secret_token(),
    ) {
        let content = format!("export {prefix}KEY={token}\n");
        let fs = findings(&content);
        prop_assume!(!fs.is_empty());
        let masked = gate::mask_findings(&content, &fs, |_| false);
        prop_assert_eq!(scan(&masked), GateVerdict::Clean, "masked still dirty: {}", masked);
    }

    /// Invariant 3: the full secret never survives masking — not through
    /// mask_findings (the stored copy) nor mask_all (the inbox evidence).
    #[test]
    fn secret_text_never_survives_masking(
        prefix in "[a-zA-Z_ ]{0,20}",
        token in secret_token(),
    ) {
        let content = format!("export {prefix}KEY={token}\n");
        let fs = findings(&content);
        prop_assume!(!fs.is_empty());
        prop_assert!(
            !gate::mask_findings(&content, &fs, |_| false).contains(&token),
            "mask_findings leaked {token}"
        );
        prop_assert!(
            !gate::mask_all(&content).contains(&token),
            "mask_all leaked {token}"
        );
    }

    /// Invariant 4: fingerprints are a deterministic 16-char hex digest
    /// of the secret text.
    #[test]
    fn fingerprints_are_stable_hex(secret in any::<String>()) {
        let a = gate::fingerprint(&secret);
        let b = gate::fingerprint(&secret);
        prop_assert_eq!(&a, &b);
        prop_assert_eq!(a.len(), 16);
        prop_assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
