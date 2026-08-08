//! The secret gate: nothing reaches a commit without passing through
//! here, and it cannot be turned off — individual findings can be
//! approved or auto-redacted from the inbox, and those resolutions
//! stick via content fingerprints. Three layers: a denylist of file
//! names that are never trackable, a curated pattern set for known
//! credential shapes, and a charset-aware entropy check for the
//! anonymous pasted token. A line ending in `wukong:allow` is exempted
//! deliberately.
//!
//! Every finding carries a fingerprint — a truncated SHA-256 of the
//! exact secret text. The engine stores resolved fingerprints per file
//! and hands them back on later scans, so approving a long-lived token
//! once is enough: the same token never re-quarantines, while a
//! *rotated* token (new fingerprint) correctly does.

use regex::Regex;
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub rule: &'static str,
    /// 1-based line number in the scanned content.
    pub line: usize,
    /// Byte span of the secret within its line.
    pub start: usize,
    pub end: usize,
    /// The offending line with the secret masked down to its edges.
    pub excerpt: String,
    /// Truncated SHA-256 of the exact secret text — the stable identity
    /// a resolution attaches to.
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    Clean,
    /// Commit is held; the findings go to the inbox.
    Quarantine(Vec<Finding>),
    /// The file itself is never trackable.
    Forbidden(&'static str),
}

/// Exact file names that are never trackable, no matter the content.
const FORBIDDEN_NAMES: &[(&str, &str)] = &[
    ("id_rsa", "SSH private key"),
    ("id_dsa", "SSH private key"),
    ("id_ecdsa", "SSH private key"),
    ("id_ed25519", "SSH private key"),
    (".netrc", "plaintext credentials file"),
    ("credentials", "credentials file"),
    (".git-credentials", "plaintext git credentials"),
    ("credentials.json", "credentials file"),
    (".histfile", "shell history"),
];

/// Extensions that mark key material.
const FORBIDDEN_EXTENSIONS: &[(&str, &str)] = &[
    ("pem", "PEM key material"),
    ("p12", "PKCS#12 key material"),
    ("pfx", "PKCS#12 key material"),
    ("keystore", "key store"),
    ("jks", "Java key store"),
];

/// Is this file forbidden by name alone? Exact names, key-material
/// extensions, `.env` and its secret-bearing variants (`.env.local`
/// yes, `.env.example` no), and shell history. Deliberately NOT
/// substring matching: `id_rsa.pub` and `.env.example` are fine.
fn forbidden(name: &str) -> Option<&'static str> {
    for (exact, why) in FORBIDDEN_NAMES {
        if name == *exact {
            return Some(why);
        }
    }
    if let Some(ext) = name.rsplit_once('.').map(|(_, e)| e) {
        for (fext, why) in FORBIDDEN_EXTENSIONS {
            if ext == *fext {
                return Some(why);
            }
        }
    }
    if name == ".env"
        || (name.starts_with(".env.")
            && !matches!(
                name.trim_start_matches(".env."),
                "example" | "sample" | "template" | "dist"
            ))
    {
        return Some("environment secrets file");
    }
    if name.ends_with("_history") {
        return Some("shell history");
    }
    None
}

static RULES: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    [
        ("private key block", r"-----BEGIN (RSA |EC |DSA |OPENSSH |PGP )?PRIVATE KEY"),
        ("AWS access key", r"\bAKIA[0-9A-Z]{16}\b"),
        ("GitHub token", r"\b(gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,})"),
        ("GitLab token", r"\bglpat-[A-Za-z0-9_-]{20,}"),
        ("Stripe live key", r"\b[sr]k_live_[A-Za-z0-9]{16,}"),
        ("Slack token", r"\bxox[baprs]-[A-Za-z0-9-]{10,}"),
        ("Slack webhook", r"hooks\.slack\.com/services/T[A-Za-z0-9/]{20,}"),
        ("Anthropic key", r"\bsk-ant-[A-Za-z0-9_-]{16,}"),
        ("OpenAI key", r"\bsk-(proj-)?[A-Za-z0-9_-]{32,}"),
        ("Google API key", r"\bAIza[A-Za-z0-9_-]{35}"),
        ("npm token", r"\bnpm_[A-Za-z0-9]{36}\b"),
        ("SendGrid key", r"\bSG\.[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{16,}"),
        ("DigitalOcean token", r"\bdop_v1_[a-f0-9]{64}\b"),
        ("Twilio key", r"\bSK[a-f0-9]{32}\b"),
        ("JWT", r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\."),
        // Assignment to a secret-ish variable. No \b around the key
        // name: underscore is a word character, so \b(token)\b can
        // never match FOO_TOKEN= — the single most common shape in a
        // dotfile. The value is validated in code (see
        // `plausible_secret_value`) to keep paths and $VARs out.
        (
            "credential assignment",
            r#"(?i)(?:^|[\s"'])[A-Za-z0-9_]*(?:api[_-]?key|secret|token|passwd|password|auth)[A-Za-z0-9_]*\s*[:=]\s*["']?([A-Za-z0-9+/=_\-.]{12,})"#,
        ),
    ]
    .into_iter()
    .map(|(name, pattern)| (name, Regex::new(pattern).expect("rule compiles")))
    .collect()
});

const ASSIGNMENT_RULE: &str = "credential assignment";

static UUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")
        .expect("uuid compiles")
});

/// Scan file content before it may be mirrored and committed. Returns
/// every finding on every line; the caller filters against its stored
/// allowances.
#[must_use]
pub fn scan(path: &Path, content: &str) -> GateVerdict {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if let Some(why) = forbidden(&name) {
        return GateVerdict::Forbidden(why);
    }

    // Binary content: line rules over lossy text would spray entropy
    // false positives. Policy: binaries pass the content gate (the
    // forbidden-name layer still applies) — documented in AGENTS.md.
    if content.bytes().take(8192).any(|b| b == 0) {
        return GateVerdict::Clean;
    }

    let mut findings = Vec::new();
    for (ix, line) in content.lines().enumerate() {
        findings.extend(scan_line(line, ix + 1));
    }

    if findings.is_empty() {
        GateVerdict::Clean
    } else {
        GateVerdict::Quarantine(findings)
    }
}

/// All findings on one line: every rule match plus non-overlapping
/// entropy spans. One line can hold several distinct secrets — each
/// gets its own finding and fingerprint.
fn scan_line(line: &str, line_no: usize) -> Vec<Finding> {
    if line.trim_end().ends_with("wukong:allow") {
        return Vec::new();
    }
    let mut spans: Vec<(&'static str, usize, usize)> = Vec::new();
    for (rule, regex) in RULES.iter() {
        for caps in regex.captures_iter(line) {
            // The assignment rule masks only its value group; other
            // rules mask the whole match.
            let m = if *rule == ASSIGNMENT_RULE {
                match caps.get(1) {
                    Some(v) if plausible_secret_value(v.as_str()) => v,
                    _ => continue,
                }
            } else {
                caps.get(0).expect("match exists")
            };
            if !overlaps(&spans, m.start(), m.end()) {
                spans.push((rule, m.start(), m.end()));
            }
        }
    }
    for (start, end) in high_entropy_spans(line) {
        if !overlaps(&spans, start, end) {
            spans.push(("high-entropy string", start, end));
        }
    }
    spans.sort_by_key(|&(_, start, _)| start);
    spans
        .into_iter()
        .map(|(rule, start, end)| Finding {
            rule,
            line: line_no,
            start,
            end,
            excerpt: mask(line, start, end),
            fingerprint: fingerprint(&line[start..end]),
        })
        .collect()
}

fn overlaps(spans: &[(&str, usize, usize)], start: usize, end: usize) -> bool {
    spans.iter().any(|&(_, s, e)| start < e && s < end)
}

/// Truncated SHA-256 of the secret text: stable across the file moving
/// or the line shifting, distinct the moment the token rotates. The
/// database stores this, never the secret.
#[must_use]
pub fn fingerprint(secret: &str) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(secret.as_bytes());
    digest[..8]
        .iter()
        .fold(String::with_capacity(16), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// Does an assignment's value look like an actual secret rather than a
/// path, a $VAR reference, or a word? Rules keep `PASSWORD_STORE_DIR=`
/// and friends quiet.
fn plausible_secret_value(value: &str) -> bool {
    if value.starts_with('/') || value.starts_with('.') || value.starts_with('~') {
        return false; // a path
    }
    if value.matches('/').count() >= 3 {
        return false; // a deep path missed above
    }
    // Demand some evidence of opacity: at least two character classes
    // among lower/upper/digit, or length ≥ 20 in one class.
    let classes = [
        value.chars().any(|c| c.is_ascii_lowercase()),
        value.chars().any(|c| c.is_ascii_uppercase()),
        value.chars().any(|c| c.is_ascii_digit()),
    ]
    .iter()
    .filter(|&&x| x)
    .count();
    classes >= 2 || value.len() >= 20
}

/// Long unbroken tokens with entropy near their charset's ceiling: the
/// shape of a pasted credential nothing else recognizes. Thresholds
/// are charset-aware — a fixed 4.2 bar is mathematically unreachable
/// for hex, whose ceiling is 4.0.
fn high_entropy_spans(line: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut cursor = 0;
    for token in line.split(|c: char| c.is_whitespace() || "\"'=:,()<>[]{}".contains(c)) {
        let start = match line[cursor..].find(token) {
            Some(off) if !token.is_empty() => cursor + off,
            _ => continue,
        };
        cursor = start + token.len();
        if entropy_secret(token) {
            out.push((start, start + token.len()));
        }
    }
    out
}

fn entropy_secret(token: &str) -> bool {
    if token.len() < 32 {
        return false;
    }
    // Structure, not secrets: paths and $VAR references, dotted names
    // (domains, files, versions), SSH public key blobs (the wire
    // format starts AAAA — signing keys legitimately live in
    // .gitconfig), and UUIDs.
    if token.starts_with('/')
        || token.starts_with('~')
        || token.starts_with('$')
        || token.starts_with('.')
        || token.contains('.')
        || token.starts_with("AAAA")
        || UUID.is_match(token)
    {
        return false;
    }
    if token.matches('/').count() >= 3 {
        return false; // deep path
    }
    let hex = token.chars().all(|c| c.is_ascii_hexdigit());
    let base64ish = token
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "+/=_-".contains(c));
    if !base64ish {
        return false;
    }
    let h = shannon(token);
    if hex { h > 3.35 } else { h > 4.2 }
}

#[allow(clippy::cast_precision_loss)] // token lengths are far below 2^52
fn shannon(s: &str) -> f64 {
    let mut counts = [0usize; 256];
    for b in s.bytes() {
        counts[b as usize] += 1;
    }
    let len = s.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / len;
            -p * p.log2()
        })
        .sum()
}

/// Replace the span with its edges plus a mask, so a line can be shown
/// without reproducing the secret.
fn mask(line: &str, start: usize, end: usize) -> String {
    let secret = &line[start..end];
    let masked = if secret.len() > 8 {
        format!("{}……{}", &secret[..4], &secret[secret.len() - 2..])
    } else {
        "……".to_string()
    };
    format!("{}{}{}", &line[..start], masked, &line[end..])
}

/// Rewrite content with the selected findings masked. `keep` decides
/// per finding: true = the secret stays (approved), false = masked.
/// The invariant the caller relies on: rescanning the result yields no
/// finding that wasn't kept deliberately — masking replaces the secret
/// with a short elided form no rule or entropy check recognizes.
pub fn mask_findings(
    content: &str,
    findings: &[Finding],
    keep: impl Fn(&Finding) -> bool,
) -> String {
    let mut by_line: HashMap<usize, Vec<&Finding>> = HashMap::new();
    for f in findings {
        if !keep(f) {
            by_line.entry(f.line).or_default().push(f);
        }
    }
    let mut out = String::with_capacity(content.len());
    for (ix, line) in content.lines().enumerate() {
        let masked = match by_line.get(&(ix + 1)) {
            Some(targets) => {
                // Mask right-to-left so earlier spans stay valid.
                let mut sorted: Vec<&&Finding> = targets.iter().collect();
                sorted.sort_by_key(|f| std::cmp::Reverse(f.start));
                let mut line = line.to_string();
                for f in sorted {
                    line = mask(&line, f.start, f.end);
                }
                line
            }
            None => line.to_string(),
        };
        out.push_str(&masked);
        out.push('\n');
    }
    if !content.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Mask every secret the gate can see in a block of text — used before
/// diffs or file excerpts are stored as inbox evidence, so the
/// database never holds a raw secret.
#[must_use]
pub fn mask_all(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let findings = scan_line(line, 1);
        if findings.is_empty() {
            out.push_str(line);
        } else {
            let mut masked = line.to_string();
            for f in findings.iter().rev() {
                masked = mask(&masked, f.start, f.end);
            }
            out.push_str(&masked);
        }
        out.push('\n');
    }
    if !text.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn verdict(content: &str) -> GateVerdict {
        scan(&PathBuf::from(".zshrc"), content)
    }

    fn held(content: &str) -> Vec<Finding> {
        match verdict(content) {
            GateVerdict::Quarantine(f) => f,
            other => panic!("expected quarantine for {content:?}, got {other:?}"),
        }
    }

    #[test]
    fn ordinary_dotfiles_pass() {
        let zshrc = r#"
export PATH="$HOME/.local/bin:$PATH"
alias ll='eza -la'
eval "$(starship init zsh)"
export EDITOR=nvim
export GOPATH=/Users/someone/dev/go/workspaces/primary/main
export PASSWORD_STORE_DIR=/Users/someone/.password-store
export AUTH_SOCK=$XDG_RUNTIME_DIR/ssh-agent.socket
source <(fzf --zsh)
# a very long comment line that mentions the word password but assigns nothing
"#;
        assert_eq!(verdict(zshrc), GateVerdict::Clean);
    }

    #[test]
    fn public_keys_uuids_and_hashes_of_words_pass() {
        for line in [
            // SSH public keys live in dotfiles legitimately.
            "user.signingkey = ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKq8xyz1234567890abcdefghijklmnopqrstuv",
            "machine-uuid: 550e8400-e29b-41d4-a716-446655440000",
            // Low-entropy repetition is not a secret.
            "marker = deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        ] {
            assert_eq!(verdict(line), GateVerdict::Clean, "{line}");
        }
    }

    #[test]
    fn known_credential_shapes_quarantine() {
        for line in [
            "export GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz012345",
            "aws_key = AKIAIOSFODNN7EXAMPLE",
            "export ANTHROPIC_API_KEY=sk-ant-abc123def456ghi789",
            "-----BEGIN OPENSSH PRIVATE KEY-----",
            "api_key: 'abcdef0123456789abcdef0123456789'",
            "export GITLAB_PAT=glpat-XyZ123abcDEF456ghi78",
            "url = https://hooks.slack.com/services/T0AAAA1BBB/B0CCCC2DDD/x1y2z3a4b5c6d7e8f9",
            "key=AIzaSyD4iE7xn1qLmO2pQr8tUvWxYz0aBcDeFgH",
        ] {
            let f = held(line);
            assert!(
                !f[0]
                    .excerpt
                    .contains("ghp_abcdefghijklmnopqrstuvwxyz012345"),
                "excerpt leaks: {}",
                f[0].excerpt
            );
        }
    }

    #[test]
    fn underscore_prefixed_names_are_caught() {
        // \b(token)\b can never match FOO_TOKEN=; the rewritten rule must.
        for line in [
            "export NPM_TOKEN=abc123DEF456ghi7",
            "export MY_APP_SECRET=qwertyASDFGH123456",
            "aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "export SOME_AUTH_TOKEN=opaquevaluetwentychars99",
        ] {
            assert!(
                matches!(verdict(line), GateVerdict::Quarantine(_)),
                "missed: {line}"
            );
        }
    }

    #[test]
    fn entropy_catches_hex_base64_and_base62() {
        for line in [
            // 64-char random hex: ceiling 4.0, so the old 4.2 bar could never fire.
            "export API_HASH=9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            // base64 containing '/' — previously excluded as \"path-ish\".
            "export S3_KEY=dGhpcy9pcy9hL3Rlc3Qvc2VjcmV0K3ZhbHVlPT0x",
            "export MYSTERY=aG93IGRpZCB5b3UgZmluZCB0aGlzIHNlY3JldD8hPz8",
        ] {
            assert!(
                matches!(verdict(line), GateVerdict::Quarantine(_)),
                "missed: {line}"
            );
        }
    }

    #[test]
    fn multiple_secrets_on_one_line_all_found() {
        let line =
            "export GH=ghp_abcdefghijklmnopqrstuvwxyz012345 ANT=sk-ant-abc123def456ghi789xyz";
        let f = held(line);
        assert_eq!(f.len(), 2, "{f:#?}");
        let masked = mask_findings(line, &f, |_| false);
        assert!(!masked.contains("ghp_abcdefghijklmnopqrstuvwxyz012345"));
        assert!(!masked.contains("sk-ant-abc123def456ghi789xyz"));
        // The redacted output must itself scan clean.
        assert_eq!(verdict(&masked), GateVerdict::Clean, "{masked}");
    }

    #[test]
    fn mask_findings_keeps_approved_spans() {
        let line =
            "export GH=ghp_abcdefghijklmnopqrstuvwxyz012345 ANT=sk-ant-abc123def456ghi789xyz";
        let f = held(line);
        let keep_fp = f[0].fingerprint.clone();
        let out = mask_findings(line, &f, |x| x.fingerprint == keep_fp);
        assert!(out.contains("ghp_abcdefghijklmnopqrstuvwxyz012345"));
        assert!(!out.contains("sk-ant-abc123def456ghi789xyz"));
    }

    #[test]
    fn fingerprints_are_stable_and_rotation_sensitive() {
        let a1 = held("export T_TOKEN=abcdef0123456789abcdef")[0]
            .fingerprint
            .clone();
        let a2 = held("   export T_TOKEN=abcdef0123456789abcdef   # moved")[0]
            .fingerprint
            .clone();
        let b = held("export T_TOKEN=fedcba9876543210fedcba")[0]
            .fingerprint
            .clone();
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
    }

    #[test]
    fn allow_marker_is_respected() {
        let line = "export GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz012345 # wukong:allow";
        assert_eq!(verdict(line), GateVerdict::Clean);
    }

    #[test]
    fn mask_all_scrubs_diff_text() {
        let diff =
            "@@ -1 +1 @@\n-export A=1\n+export GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz012345";
        let masked = mask_all(diff);
        assert!(!masked.contains("ghp_abcdefghijklmnopqrstuvwxyz012345"));
        assert!(masked.contains("@@ -1 +1 @@"));
    }

    #[test]
    fn forbidden_names_are_exact_not_substring() {
        for name in [
            ".ssh/id_ed25519",
            "certs/server.pem",
            ".env",
            ".env.local",
            ".netrc",
            ".zsh_history",
            ".aws/credentials",
        ] {
            assert!(
                matches!(scan(&PathBuf::from(name), "x"), GateVerdict::Forbidden(_)),
                "{name} should be forbidden"
            );
        }
        for name in [".ssh/id_ed25519.pub", ".env.example", "environment.md"] {
            assert!(
                !matches!(scan(&PathBuf::from(name), "x"), GateVerdict::Forbidden(_)),
                "{name} should be allowed"
            );
        }
    }

    #[test]
    fn binary_content_passes_content_rules() {
        let mut bin = String::from("plist\0\0");
        bin.push_str("Zm9vYmFyYmF6cXV4QUJDREVGMTIzNDU2Nzg5MHh5eg");
        assert_eq!(verdict(&bin), GateVerdict::Clean);
    }
}
