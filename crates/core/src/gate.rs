//! The secret gate: nothing reaches a commit without passing through
//! here, and it cannot be turned off — individual findings can be
//! approved or auto-redacted from the inbox, and those resolutions
//! stick via content fingerprints. Three layers: a denylist of file
//! names that are never trackable, a curated pattern set for known
//! credential shapes, and a charset-aware entropy check for the
//! anonymous pasted token. A line ending in `wukong:allow` is exempted
//! from QUARANTINE deliberately — but never from evidence masking:
//! what the inbox stores is masked regardless of markers.
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
    /// The offending line with EVERY detected secret on it masked —
    /// not just this finding's own span, so an excerpt can never leak
    /// a line-mate.
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

/// A byte-level scan: the verdict plus the text it was computed
/// against, which is NOT always a lossy UTF-8 view — UTF-16 files are
/// decoded first so their secrets are visible to the rules.
#[derive(Debug)]
pub struct Scanned {
    pub verdict: GateVerdict,
    /// The text the findings' line/span offsets refer to.
    pub text: String,
    /// True when `text` is not byte-identical to the input (UTF-16
    /// decode or lossy replacement). Redaction must not proceed from
    /// such text — spans don't map back to the original bytes.
    pub reencoded: bool,
}

/// Exact file names that are never trackable, no matter the content.
const FORBIDDEN_NAMES: &[(&str, &str)] = &[
    (".netrc", "plaintext credentials file"),
    ("credentials", "credentials file"),
    (".git-credentials", "plaintext git credentials"),
    ("credentials.json", "credentials file"),
    (".histfile", "shell history"),
    (".pgpass", "plaintext database passwords"),
    (".my.cnf", "database credentials file"),
    (".pypirc", "package registry credentials"),
];

/// Name prefixes for private key files (covers `id_rsa_work` etc.);
/// `.pub` halves are fine.
const FORBIDDEN_KEY_PREFIXES: &[&str] = &["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"];

/// Extensions that mark key material or credential stores.
const FORBIDDEN_EXTENSIONS: &[(&str, &str)] = &[
    ("pem", "PEM key material"),
    ("p12", "PKCS#12 key material"),
    ("pfx", "PKCS#12 key material"),
    ("p8", "PKCS#8 key material"),
    ("der", "DER key material"),
    ("keystore", "key store"),
    ("jks", "Java key store"),
    ("kdbx", "password database"),
    ("kbx", "GnuPG keybox"),
    ("tfstate", "Terraform state (often holds secrets)"),
    ("tfvars", "Terraform variables (often holds secrets)"),
];

/// Path suffixes that identify credential files whose *name* alone is
/// too generic to deny.
const FORBIDDEN_PATH_SUFFIXES: &[(&str, &str)] = &[
    (".kube/config", "Kubernetes credentials"),
    (".docker/config.json", "Docker registry credentials"),
];

/// Is this file forbidden by name or path alone? Exact names,
/// key-material prefixes/extensions, `.env` and its secret-bearing
/// variants (`.env.local` yes, `.env.example` no), shell history, and
/// a few well-known credential paths. Deliberately NOT substring
/// matching: `id_rsa.pub` and `.env.example` are fine.
fn forbidden(path: &Path, name: &str) -> Option<&'static str> {
    for (exact, why) in FORBIDDEN_NAMES {
        if name == *exact {
            return Some(why);
        }
    }
    for prefix in FORBIDDEN_KEY_PREFIXES {
        if name.starts_with(prefix)
            && !std::path::Path::new(&name)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("pub"))
        {
            return Some("SSH private key");
        }
    }
    if let Some(ext) = name.rsplit_once('.').map(|(_, e)| e) {
        for (fext, why) in FORBIDDEN_EXTENSIONS {
            if ext == *fext {
                return Some(why);
            }
        }
    }
    if name == ".env" {
        return Some("environment secrets file");
    }
    if let Some(variant) = name.strip_prefix(".env.")
        && !matches!(variant, "example" | "sample" | "template" | "dist")
    {
        return Some("environment secrets file");
    }
    if name.ends_with("_history") {
        return Some("shell history");
    }
    let path_str = path.to_string_lossy();
    for (suffix, why) in FORBIDDEN_PATH_SUFFIXES {
        if path_str.ends_with(suffix) {
            return Some(why);
        }
    }
    None
}

/// A detection rule: the regex plus which capture group is the secret
/// (0 = the whole match).
struct Rule {
    name: &'static str,
    regex: Regex,
    secret_group: usize,
}

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    let rule = |name, pattern: &str, secret_group| Rule {
        name,
        regex: Regex::new(pattern).expect("rule compiles"),
        secret_group,
    };
    vec![
        rule(
            "private key block",
            r"-----BEGIN (RSA |EC |DSA |OPENSSH |PGP )?PRIVATE KEY",
            0,
        ),
        rule("AWS access key", r"\bAKIA[0-9A-Z]{16}\b", 0),
        rule(
            "GitHub token",
            r"\b(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,})",
            0,
        ),
        rule("GitLab token", r"\bglpat-[A-Za-z0-9_-]{20,}", 0),
        rule("Stripe live key", r"\b[sr]k_live_[A-Za-z0-9]{16,}", 0),
        rule("Slack token", r"\bxox[abceprs]-[A-Za-z0-9-]{10,}", 0),
        rule(
            "Slack webhook",
            r"hooks\.slack\.com/services/T[A-Za-z0-9/]{20,}",
            0,
        ),
        rule("Anthropic key", r"\bsk-ant-[A-Za-z0-9_-]{16,}", 0),
        rule("OpenAI key", r"\bsk-(?:proj-)?[A-Za-z0-9_-]{32,}", 0),
        rule("Google API key", r"\bAIza[A-Za-z0-9_-]{35}", 0),
        rule("age secret key", r"\bAGE-SECRET-KEY-1[A-Z0-9]{20,}", 0),
        rule("npm token", r"\bnpm_[A-Za-z0-9]{36}\b", 0),
        rule(
            "SendGrid key",
            r"\bSG\.[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{16,}",
            0,
        ),
        rule("DigitalOcean token", r"\bdop_v1_[a-f0-9]{64}\b", 0),
        rule("Twilio key", r"\bSK[a-f0-9]{32}\b", 0),
        // Signature included: leaving it behind would commit a
        // forgeable-checkable fragment of the token.
        rule(
            "JWT",
            r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]*",
            0,
        ),
        // Credentials embedded in URLs: scheme://user:password@host.
        // Only the password is the secret; the host stays readable in
        // evidence.
        rule("credential URL", r"://[^/\s:@]{1,64}:([^/\s@]{6,})@", 1),
        // Authorization header / bearer shape.
        rule(
            "bearer token",
            r"(?i)\bbearer\s+([A-Za-z0-9_\-.=+/]{16,})",
            1,
        ),
        // Assignment to a secret-ish variable. The left boundary is
        // "anything that is not a word character" — NOT a whitelist —
        // so `+KEY=…` in a diff, `"key":` in JSON, `{key:`, `--key=`,
        // and unicode prefixes all still anchor. No \b around the key
        // name: underscore is a word character, so \b(token)\b can
        // never match FOO_TOKEN=. The value is validated in code (see
        // `plausible_secret_value`).
        rule(
            "credential assignment",
            r#"(?i)(?:^|[^a-z0-9_])[a-z0-9_]*(?:api[_-]?key|secret|token|passwd|password|pass|pwd|auth|credential|key)[a-z0-9_]*["']?\s*[:=]\s*["']?([^\s"']{12,})"#,
            1,
        ),
    ]
});

static UUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")
        .expect("uuid compiles")
});

/// The inline exemption marker: a line ending with it is not
/// quarantined. It never exempts evidence masking, and the engine
/// records its use in the commit audit trail.
pub const ALLOW_MARKER: &str = "wukong:allow";

/// Scan file bytes before they may be mirrored and committed. Handles
/// text, UTF-16 (decoded so its secrets are visible), and binary
/// (rules still run over the lossy text; only the entropy layer is
/// skipped, since binary garbage would trip it constantly).
#[must_use]
pub fn scan_bytes(path: &Path, bytes: &[u8]) -> Scanned {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if let Some(why) = forbidden(path, &name) {
        return Scanned {
            verdict: GateVerdict::Forbidden(why),
            text: String::new(),
            reencoded: true,
        };
    }

    if let Some(decoded) = decode_utf16(bytes) {
        let verdict = scan_text(&decoded, true);
        return Scanned {
            verdict,
            text: decoded,
            reencoded: true,
        };
    }

    let binary = bytes.iter().take(8192).any(|&b| b == 0);
    let text = String::from_utf8_lossy(bytes);
    let reencoded = text.as_bytes() != bytes;
    let verdict = scan_text(&text, !binary);
    Scanned {
        verdict,
        text: text.into_owned(),
        reencoded,
    }
}

/// Scan string content (the str-level entry point; `scan_bytes` is the
/// byte-level one the engine uses).
#[must_use]
pub fn scan(path: &Path, content: &str) -> GateVerdict {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if let Some(why) = forbidden(path, &name) {
        return GateVerdict::Forbidden(why);
    }
    if content.bytes().take(8192).any(|b| b == 0) {
        return scan_text(content, false);
    }
    scan_text(content, true)
}

fn scan_text(content: &str, with_entropy: bool) -> GateVerdict {
    let mut findings = Vec::new();
    for (ix, line) in content.lines().enumerate() {
        findings.extend(scan_line(line, ix + 1, with_entropy));
    }
    if findings.is_empty() {
        GateVerdict::Clean
    } else {
        GateVerdict::Quarantine(findings)
    }
}

/// All findings on one line: every rule match plus non-overlapping
/// entropy spans. One line can hold several distinct secrets — each
/// gets its own finding and fingerprint, and every excerpt masks the
/// WHOLE line's spans. A line ending in the allow marker yields no
/// findings (that's the exemption); evidence masking uses
/// `spans_for_masking` instead, which ignores the marker.
fn scan_line(line: &str, line_no: usize, with_entropy: bool) -> Vec<Finding> {
    if line.trim_end().ends_with(ALLOW_MARKER) {
        return Vec::new();
    }
    let spans = line_spans(line, with_entropy);
    if spans.is_empty() {
        return Vec::new();
    }
    let excerpt = mask_spans(line, &spans);
    spans
        .into_iter()
        .map(|(rule, start, end)| Finding {
            rule,
            line: line_no,
            start,
            end,
            excerpt: excerpt.clone(),
            fingerprint: fingerprint(&line[start..end]),
        })
        .collect()
}

/// The raw secret spans on a line, allow-marker or not — the masking
/// side of the gate, which must never be bypassable.
fn line_spans(line: &str, with_entropy: bool) -> Vec<(&'static str, usize, usize)> {
    let mut spans: Vec<(&'static str, usize, usize)> = Vec::new();
    for rule in RULES.iter() {
        for caps in rule.regex.captures_iter(line) {
            let m = match caps.get(rule.secret_group) {
                Some(m) if rule.secret_group == 0 || plausible_secret_value(m.as_str()) => m,
                _ => continue,
            };
            if !overlaps(&spans, m.start(), m.end()) {
                spans.push((rule.name, m.start(), m.end()));
            }
        }
    }
    if with_entropy && line.len() <= 65_536 {
        for (start, end) in high_entropy_spans(line) {
            if !overlaps(&spans, start, end) {
                spans.push(("high-entropy string", start, end));
            }
        }
    }
    spans.sort_by_key(|&(_, start, _)| start);
    spans
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
/// path, a $VAR reference, or a word? Keeps `PASSWORD_STORE_DIR=` and
/// `AUTH_SOCK=$…` quiet while catching punctuation-bearing passwords.
fn plausible_secret_value(value: &str) -> bool {
    if value.starts_with('~') || value.starts_with('.') || value.starts_with('$') {
        return false; // a path or a variable reference
    }
    // Only a LEADING slash marks a path candidate; a slash mid-token
    // is normal base64. (Random base64 can produce short segments, so
    // structure alone cannot separate the two — found by proptest.)
    if value.starts_with('/') && path_like(value) {
        return false;
    }
    // Short pure-hex values are overwhelmingly key IDs and digests
    // (GPG signing keys in .gitconfig, commit pins); real hex secrets
    // are 32+ chars and the entropy layer owns those.
    if value.len() < 32 && value.chars().all(|c| c.is_ascii_hexdigit()) {
        return false;
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

/// Paths have several slashes and SHORT segments between them;
/// base64 material with incidental slashes has long runs (a random
/// base64 segment averages ~64 chars between slashes). Found by the
/// property suite: a 40-char base64 secret with three slashes must not
/// be waved through as a "deep path".
fn path_like(token: &str) -> bool {
    token.matches('/').count() >= 2
        && token.split('/').all(|seg| {
            seg.len() <= 16
                && seg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._@-".contains(c))
        })
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
    if token.starts_with('~')
        || token.starts_with('$')
        || token.starts_with('.')
        || token.contains('.')
        || token.starts_with("AAAA")
        || UUID.is_match(token)
    {
        return false;
    }
    if token.starts_with('/') && path_like(token) {
        return false;
    }
    let hex = token.chars().all(|c| c.is_ascii_hexdigit());
    let base64ish = token
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "+/=_-".contains(c));
    if !base64ish {
        return false;
    }
    // Slash-bearing tokens get a higher bar: real paths are wordy and
    // sit below ~4.3 bits, random base64 lands ~4.5-4.9.
    let h = shannon(token);
    if hex {
        h > 3.35
    } else if token.contains('/') {
        h > 4.45
    } else {
        h > 4.2
    }
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

/// Replace one span with its leading edge plus a mask. Four leading
/// characters are enough to recognize a token; the old trailing two
/// only shrank the brute-force space of what gets committed on redact.
fn mask(line: &str, start: usize, end: usize) -> String {
    let secret = &line[start..end];
    let masked = if secret.len() > 8 {
        format!("{}……", &secret[..4])
    } else {
        "……".to_string()
    };
    format!("{}{}{}", &line[..start], masked, &line[end..])
}

/// Mask a set of spans on one line, right-to-left so byte offsets stay
/// valid.
fn mask_spans(line: &str, spans: &[(&'static str, usize, usize)]) -> String {
    let mut out = line.to_string();
    for &(_, start, end) in spans.iter().rev() {
        out = mask(&out, start, end);
    }
    out
}

/// Split content into (line, terminator) pairs so masking can rebuild
/// the exact original line endings — a CRLF file must not come out of
/// redaction silently LF-normalized.
fn split_lines(content: &str) -> impl Iterator<Item = (&str, &str)> {
    content.split_inclusive('\n').map(|seg| {
        if let Some(body) = seg.strip_suffix("\r\n") {
            (body, "\r\n")
        } else if let Some(body) = seg.strip_suffix('\n') {
            (body, "\n")
        } else {
            (seg, "")
        }
    })
}

/// Rewrite content with the selected findings masked. `keep` decides
/// per finding: true = the secret stays (approved), false = masked.
/// The invariant the caller relies on: rescanning the result yields no
/// finding that wasn't kept deliberately — masking replaces the secret
/// with a short elided form no rule or entropy check recognizes. Line
/// endings (LF/CRLF) are preserved byte-for-byte.
#[must_use]
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
    for (ix, (line, ending)) in split_lines(content).enumerate() {
        match by_line.get(&(ix + 1)) {
            Some(targets) => {
                let mut sorted: Vec<&&Finding> = targets.iter().collect();
                sorted.sort_by_key(|f| std::cmp::Reverse(f.start));
                let mut masked = line.to_string();
                for f in sorted {
                    masked = mask(&masked, f.start, f.end);
                }
                out.push_str(&masked);
            }
            None => out.push_str(line),
        }
        out.push_str(ending);
    }
    out
}

/// Mask every secret the gate can see in a block of text — used before
/// diffs or file excerpts are stored as inbox evidence, so the
/// database never holds a raw secret. Unlike the quarantine side, this
/// deliberately IGNORES `wukong:allow` markers: an allowed secret may
/// be committed, but evidence is still evidence.
#[must_use]
pub fn mask_all(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (line, ending) in split_lines(text) {
        let spans = line_spans(line, true);
        if spans.is_empty() {
            out.push_str(line);
        } else {
            out.push_str(&mask_spans(line, &spans));
        }
        out.push_str(ending);
    }
    out
}

/// How many lines of this content invoke the allow marker — the
/// engine's audit trail for commits that carry exempted secrets.
#[must_use]
pub fn allow_marker_count(content: &str) -> usize {
    content
        .lines()
        .filter(|l| l.trim_end().ends_with(ALLOW_MARKER))
        .count()
}

/// Decode UTF-16 content (BOM'd, or NUL-dense in the alternating
/// pattern ASCII-heavy UTF-16 produces). Without this, a UTF-16
/// `profile.ps1` full of tokens would scan "binary-clean".
fn decode_utf16(bytes: &[u8]) -> Option<String> {
    let (le, payload) = match bytes {
        [0xFF, 0xFE, rest @ ..] => (true, rest),
        [0xFE, 0xFF, rest @ ..] => (false, rest),
        _ => {
            // Heuristic: mostly-ASCII UTF-16 has a NUL in every other
            // byte. Sample the first 4KB.
            let sample = &bytes[..bytes.len().min(4096)];
            if sample.len() < 8 {
                return None;
            }
            let odd_nuls = sample
                .iter()
                .skip(1)
                .step_by(2)
                .filter(|&&b| b == 0)
                .count();
            let even_nuls = sample.iter().step_by(2).filter(|&&b| b == 0).count();
            let half = sample.len() / 2;
            if odd_nuls * 10 >= half * 8 && even_nuls * 10 < half {
                (true, bytes)
            } else if even_nuls * 10 >= half * 8 && odd_nuls * 10 < half {
                (false, bytes)
            } else {
                return None;
            }
        }
    };
    let units: Vec<u16> = payload
        .chunks_exact(2)
        .map(|c| {
            if le {
                u16::from_le_bytes([c[0], c[1]])
            } else {
                u16::from_be_bytes([c[0], c[1]])
            }
        })
        .collect();
    Some(String::from_utf16_lossy(&units))
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
    fn public_keys_uuids_and_key_ids_pass() {
        for line in [
            "user.signingkey = ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKq8xyz1234567890abcdefghijklmnopqrstuv",
            "machine-uuid: 550e8400-e29b-41d4-a716-446655440000",
            "marker = deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            // GPG key id in .gitconfig: pure hex under 32 chars.
            "signingkey = 3AA5C34371567BD2",
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
            "api_key: 'abcdef0123456789abcdef0123456789Z'",
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
    fn review_found_bypasses_are_caught() {
        for line in [
            // Diff-prefixed line: '+' is not [\s"'], the old anchor missed it.
            "+API_PASSWORD=hunter2trombone99",
            "-API_PASSWORD=hunter2trombone99",
            // Quoted JSON keys.
            r#""api_key": "aB3dEf6GhJ9kLm2NpQ5rS""#,
            "'secret': 'aB3dEf6GhJ9kLm2NpQ5rS'",
            // Brace/flag/unicode prefixes.
            "{api_token:aB3dEf6GhJ9kLm2NpQ5rS}",
            "--api-token=aB3dEf6GhJ9kLm2NpQ5rS",
            "設定api_key=aB3dEf6GhJ9kLm2NpQ5rS",
            // Punctuation-bearing password (the most common real shape).
            "export DB_PASSWORD=Tr0ub4dor&3xtra!suffix",
            // Credential URLs — previously invisible to every layer.
            "export DATABASE_URL=postgres://admin:hunter2pass@db.internal:5432/app",
            "git_remote = https://deploy:s3cr3tpassword@example.com/repo.git",
            // Bearer tokens.
            "Authorization: Bearer aB3dEf6GhJ9kLm2NpQ5rS7tUv",
            // Bare KEY= names.
            "export SIGNING_KEY=aB3dEf6GhJ9kLm2NpQ5rS",
            "ENCRYPTION_KEY: aB3dEf6GhJ9kLm2NpQ5rS",
        ] {
            assert!(
                matches!(verdict(line), GateVerdict::Quarantine(_)),
                "missed: {line}"
            );
        }
    }

    #[test]
    fn credential_url_masks_only_the_password() {
        let f = held("export DATABASE_URL=postgres://admin:hunter2pass@db.internal:5432/app");
        assert!(!f[0].excerpt.contains("hunter2pass"), "{}", f[0].excerpt);
        assert!(f[0].excerpt.contains("db.internal"), "{}", f[0].excerpt);
    }

    #[test]
    fn entropy_catches_hex_base64_and_base62() {
        for line in [
            "export API_HASH=9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
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
    fn multi_secret_line_excerpt_never_leaks_a_linemate() {
        let line =
            "export GH=ghp_abcdefghijklmnopqrstuvwxyz012345 ANT=sk-ant-abc123def456ghi789xyz";
        let f = held(line);
        assert_eq!(f.len(), 2, "{f:#?}");
        // EVERY excerpt must mask EVERY secret on the line.
        for finding in &f {
            assert!(
                !finding
                    .excerpt
                    .contains("ghp_abcdefghijklmnopqrstuvwxyz012345"),
                "excerpt leaks gh token: {}",
                finding.excerpt
            );
            assert!(
                !finding.excerpt.contains("sk-ant-abc123def456ghi789xyz"),
                "excerpt leaks anthropic key: {}",
                finding.excerpt
            );
        }
        let masked = mask_findings(line, &f, |_| false);
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
    fn masking_preserves_crlf() {
        let content = "export A=1\r\nexport T=ghp_abcdefghijklmnopqrstuvwxyz012345\r\n";
        let f = match scan(&PathBuf::from(".zshrc"), content) {
            GateVerdict::Quarantine(f) => f,
            other => panic!("{other:?}"),
        };
        let masked = mask_findings(content, &f, |_| false);
        assert_eq!(masked.matches("\r\n").count(), 2, "{masked:?}");
        assert!(!masked.contains("ghp_abcdefghijklmnopqrstuvwxyz012345"));
    }

    #[test]
    fn mask_all_scrubs_diff_text_including_column_zero_assignments() {
        let diff = "@@ -1 +1 @@\n-export A=1\n+API_PASSWORD=hunter2trombone99\n+export GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz012345";
        let masked = mask_all(diff);
        assert!(!masked.contains("hunter2trombone99"), "{masked}");
        assert!(
            !masked.contains("ghp_abcdefghijklmnopqrstuvwxyz012345"),
            "{masked}"
        );
        assert!(masked.contains("@@ -1 +1 @@"));
    }

    #[test]
    fn allow_marker_exempts_commit_but_never_evidence() {
        let line = "export GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz012345 # wukong:allow";
        // Quarantine side: exempt.
        assert_eq!(verdict(line), GateVerdict::Clean);
        assert_eq!(allow_marker_count(line), 1);
        // Evidence side: still masked.
        let masked = mask_all(line);
        assert!(
            !masked.contains("ghp_abcdefghijklmnopqrstuvwxyz012345"),
            "evidence leaked through allow marker: {masked}"
        );
    }

    #[test]
    fn jwt_masking_includes_the_signature() {
        let line = "export JWT=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJVadQssw5c";
        let f = held(line);
        let masked = mask_findings(line, &f, |_| false);
        assert!(
            !masked.contains("SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJVadQssw5c"),
            "{masked}"
        );
    }

    #[test]
    fn stored_mask_keeps_no_tail() {
        let f = held("export T=ghp_abcdefghijklmnopqrstuvwxyz012345");
        let masked = mask_findings("export T=ghp_abcdefghijklmnopqrstuvwxyz012345", &f, |_| {
            false
        });
        assert!(masked.contains("ghp_……"), "{masked}");
        assert!(!masked.contains("45"), "tail survived: {masked}");
    }

    #[test]
    fn utf16_content_is_decoded_and_scanned() {
        let text = "export GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz012345\n";
        // UTF-16LE with BOM.
        let mut bytes = vec![0xFF, 0xFE];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let scanned = scan_bytes(&PathBuf::from("profile.ps1"), &bytes);
        assert!(
            matches!(scanned.verdict, GateVerdict::Quarantine(_)),
            "utf16 bypass: {:?}",
            scanned.verdict
        );
        assert!(scanned.reencoded);

        // BOM-less UTF-16LE (the alternating-NUL heuristic).
        let scanned = scan_bytes(&PathBuf::from("profile.ps1"), &bytes[2..]);
        assert!(
            matches!(scanned.verdict, GateVerdict::Quarantine(_)),
            "bomless utf16 bypass"
        );
    }

    #[test]
    fn binary_content_still_runs_pattern_rules() {
        let mut bin = b"\x00\x01binarydata ".to_vec();
        bin.extend_from_slice(b"ghp_abcdefghijklmnopqrstuvwxyz012345 more\x00data");
        let scanned = scan_bytes(&PathBuf::from("blob.bin"), &bin);
        assert!(
            matches!(scanned.verdict, GateVerdict::Quarantine(_)),
            "ascii secret hidden in binary passed: {:?}",
            scanned.verdict
        );
        // …but entropy noise alone does not quarantine binaries.
        let noise = b"\x00\x02plainbinary Zm9vYmFyYmF6cXV4QUJDREVGMTIzNDU2Nzg5MHh5eg".to_vec();
        let scanned = scan_bytes(&PathBuf::from("blob.bin"), &noise);
        assert_eq!(scanned.verdict, GateVerdict::Clean);
    }

    #[test]
    fn fingerprints_are_stable_and_rotation_sensitive() {
        let a1 = held("export T_TOKEN=abcdef0123456789abcdefZ")[0]
            .fingerprint
            .clone();
        let a2 = held("   export T_TOKEN=abcdef0123456789abcdefZ   # moved")[0]
            .fingerprint
            .clone();
        let b = held("export T_TOKEN=fedcba9876543210fedcbaZ")[0]
            .fingerprint
            .clone();
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
    }

    #[test]
    fn forbidden_names_are_exact_not_substring() {
        for name in [
            ".ssh/id_ed25519",
            ".ssh/id_rsa_work",
            "certs/server.pem",
            "certs/key.p8",
            "vault/secrets.kdbx",
            ".env",
            ".env.local",
            ".netrc",
            ".zsh_history",
            ".aws/credentials",
            ".pgpass",
            ".kube/config",
            ".docker/config.json",
            "infra/prod.tfvars",
        ] {
            assert!(
                matches!(scan(&PathBuf::from(name), "x"), GateVerdict::Forbidden(_)),
                "{name} should be forbidden"
            );
        }
        for name in [
            ".ssh/id_ed25519.pub",
            ".ssh/id_rsa.pub",
            ".env.example",
            "environment.md",
            "Documents/preso.key", // Keynote, not key material
            ".config/other/config.json",
        ] {
            assert!(
                !matches!(scan(&PathBuf::from(name), "x"), GateVerdict::Forbidden(_)),
                "{name} should be allowed"
            );
        }
    }
}
