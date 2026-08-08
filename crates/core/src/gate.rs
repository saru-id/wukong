//! The secret gate: nothing reaches a commit without passing through
//! here, and it cannot be turned off — only individual findings can be
//! approved from the inbox. Three layers: a hard denylist of files
//! that are never trackable (private keys, .env), a curated pattern
//! set for known credential shapes, and an entropy check for the
//! anonymous 40-character surprise. A line ending in `wukong:allow`
//! is exempted deliberately.

use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub rule: &'static str,
    /// 1-based line number in the scanned content.
    pub line: usize,
    /// The offending line with the secret masked down to its edges.
    pub excerpt: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    Clean,
    /// Commit is held; the findings go to the inbox.
    Quarantine(Vec<Finding>),
    /// The file itself is never trackable.
    Forbidden(&'static str),
}

/// File names that are never trackable, no matter what they contain.
const FORBIDDEN: &[(&str, &str)] = &[
    ("id_rsa", "SSH private key"),
    ("id_ecdsa", "SSH private key"),
    ("id_ed25519", "SSH private key"),
    (".pem", "PEM key material"),
    (".p12", "PKCS#12 key material"),
    (".keystore", "key store"),
    (".env", "environment secrets file"),
    (".netrc", "plaintext credentials file"),
    ("credentials", "credentials file"),
    ("_history", "shell history"),
];

static RULES: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    [
        ("private key block", r"-----BEGIN (RSA |EC |DSA |OPENSSH |PGP )?PRIVATE KEY"),
        ("AWS access key", r"\bAKIA[0-9A-Z]{16}\b"),
        ("GitHub token", r"\b(gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,})"),
        ("Stripe live key", r"\b[sr]k_live_[A-Za-z0-9]{16,}"),
        ("Slack token", r"\bxox[baprs]-[A-Za-z0-9-]{10,}"),
        ("Anthropic key", r"\bsk-ant-[A-Za-z0-9_-]{16,}"),
        ("OpenAI key", r"\bsk-(proj-)?[A-Za-z0-9_-]{32,}"),
        ("JWT", r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\."),
        (
            "credential assignment",
            r#"(?i)\b(api[_-]?key|secret|token|password|passwd|auth)\b\s*[:=]\s*["']?[A-Za-z0-9+/_\-.]{16,}"#,
        ),
    ]
    .into_iter()
    .map(|(name, pattern)| (name, Regex::new(pattern).expect("rule compiles")))
    .collect()
});

/// Scan file content before it may be mirrored and committed.
pub fn scan(path: &Path, content: &str) -> GateVerdict {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    for (marker, why) in FORBIDDEN {
        if name.contains(marker) {
            return GateVerdict::Forbidden(why);
        }
    }

    let mut findings = Vec::new();
    for (ix, line) in content.lines().enumerate() {
        if line.trim_end().ends_with("wukong:allow") {
            continue;
        }
        for (rule, regex) in RULES.iter() {
            if let Some(m) = regex.find(line) {
                findings.push(Finding {
                    rule,
                    line: ix + 1,
                    excerpt: mask(line, m.start(), m.end()),
                });
                break; // one finding per line is enough to hold it
            }
        }
        if !findings.iter().any(|f| f.line == ix + 1)
            && let Some(range) = high_entropy_span(line)
        {
            findings.push(Finding {
                rule: "high-entropy string",
                line: ix + 1,
                excerpt: mask(line, range.0, range.1),
            });
        }
    }

    if findings.is_empty() {
        GateVerdict::Clean
    } else {
        GateVerdict::Quarantine(findings)
    }
}

/// Replace the matched span with its edges plus a mask, so the inbox
/// can show the line without reproducing the secret.
fn mask(line: &str, start: usize, end: usize) -> String {
    let secret = &line[start..end];
    let masked = if secret.len() > 8 {
        format!("{}……{}", &secret[..4], &secret[secret.len() - 2..])
    } else {
        "……".to_string()
    };
    format!("{}{}{}", &line[..start], masked, &line[end..])
}

/// A long unbroken token with high Shannon entropy: the shape of a
/// pasted credential nothing else recognizes. PATH-ish and URL-ish
/// lines are exempt — slashes and colons mark structure, not secrets.
fn high_entropy_span(line: &str) -> Option<(usize, usize)> {
    for token in line.split(|c: char| c.is_whitespace() || "\"'=:,()".contains(c)) {
        if token.len() < 32 || token.contains('/') || token.contains('.') {
            continue;
        }
        if !token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '_' || c == '-')
        {
            continue;
        }
        if shannon(token) > 4.2 {
            let start = line.find(token).unwrap_or(0);
            return Some((start, start + token.len()));
        }
    }
    None
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn verdict(content: &str) -> GateVerdict {
        scan(&PathBuf::from(".zshrc"), content)
    }

    #[test]
    fn ordinary_dotfiles_pass() {
        let zshrc = r#"
export PATH="$HOME/.local/bin:$PATH"
alias ll='eza -la'
eval "$(starship init zsh)"
export EDITOR=nvim
source <(fzf --zsh)
# a very long comment line that mentions the word password but assigns nothing
"#;
        assert_eq!(verdict(zshrc), GateVerdict::Clean);
    }

    #[test]
    fn known_credential_shapes_quarantine() {
        for line in [
            "export GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz012345",
            "aws_key = AKIAIOSFODNN7EXAMPLE",
            "export ANTHROPIC_API_KEY=sk-ant-abc123def456ghi789",
            "-----BEGIN OPENSSH PRIVATE KEY-----",
            "api_key: 'abcdef0123456789abcdef0123456789'",
        ] {
            match verdict(line) {
                GateVerdict::Quarantine(findings) => {
                    assert_eq!(findings.len(), 1, "{line}");
                    // The excerpt never contains the full secret.
                    assert!(
                        !findings[0]
                            .excerpt
                            .contains("ghp_abcdefghijklmnopqrstuvwxyz012345"),
                        "excerpt leaks: {}",
                        findings[0].excerpt
                    );
                }
                other => panic!("expected quarantine for {line}, got {other:?}"),
            }
        }
    }

    #[test]
    fn entropy_catches_the_anonymous_token() {
        let line = "export MYSTERY=aG93IGRpZCB5b3UgZmluZCB0aGlzIHNlY3JldD8hPz8";
        assert!(matches!(verdict(line), GateVerdict::Quarantine(_)));
        // …but a long path is structure, not secret.
        let path = "export GOPATH=/Users/someone/dev/go/workspaces/primary/main";
        assert_eq!(verdict(path), GateVerdict::Clean);
    }

    #[test]
    fn allow_marker_is_respected() {
        let line = "export GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz012345 # wukong:allow";
        assert_eq!(verdict(line), GateVerdict::Clean);
    }

    #[test]
    fn forbidden_files_never_pass() {
        for name in [
            ".ssh/id_ed25519",
            "certs/server.pem",
            ".env",
            ".netrc",
            ".zsh_history",
        ] {
            assert!(matches!(
                scan(&PathBuf::from(name), "anything"),
                GateVerdict::Forbidden(_)
            ));
        }
    }
}
