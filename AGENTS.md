# AGENTS.md

Instructions for coding agents working in this repo. Read README.md
first; it is accurate.

## What this is

wukong v2: a system-governing service, NOT the old gpui setup app (that
lives at github.com/saru-id/wukong-app and locally at `../wukong-backup`).
A low-memory daemon (`wukongd`) plus a CLI/TUI (`wukong`); a GUI is a
much later phase.

## Ground rules

- Rust edition 2024, workspace lints (`unsafe_code = "forbid"`). Keep
  clippy clean at default levels.
- `crates/core` stays free of the daemon and the socket — pure domain,
  tested with `cargo nextest`. The daemon and CLI compose it.
- The **secret gate is load-bearing**: nothing reaches a commit without
  `gate::scan` clearing it. Never add a path that commits file content
  without going through the gate. New credential patterns are welcome;
  weakening the gate is not.
- The engine (`wukongd/src/engine.rs`) is the single owner of all
  mutable state. One writer, no shared locks. Keep it that way.
- Conventional Commits. Leave work uncommitted unless asked.
- Never use npm/yarn; there is no JS here.

## Hard-won gotchas

- **macOS path canonicalization.** FSEvents reports real paths
  (`/private/var/…`); `$HOME` is often the symlink form (`/var/…`).
  `paths::home()` is canonicalized once for exactly this reason, and
  `engine::resolve` canonicalizes CLI input's parent dir. If tracked
  files stop matching their events, this is why.
- **Store churn must be ignored.** The watcher sees the store's own
  `.git` writes; `touch` filters anything under the store dir and any
  path with a `.git` component. Do not remove that filter.
- **Watch roots are dynamic.** They can't all be known at boot (a
  not-yet-created sentinel, a file tracked later). The engine
  accumulates `watch_requests`; main drains them after each client
  request. Sentinels that don't exist yet are covered by watching their
  parent dir.

## Verification

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets
cargo nextest run
```

For the daemon, run the isolated live drill: point `HOME` and the three
`XDG_*` vars at a tempdir, write a `config.toml` with a bare-repo
remote, start `wukongd`, then exercise track → edit → auto-commit,
paste a fake credential (must quarantine, must NOT enter git — grep the
store and remote to prove it), and change a sentinel (must appear in the
inbox). Never run the daemon against the real `$HOME` while testing.

## Runtime paths

- Config: `~/.config/wukong/config.toml`
- Store repo + database: `~/.local/share/wukong/{store,wukong.db}`
- Socket + logs: `~/.local/state/wukong/`
- LaunchAgent: `~/Library/LaunchAgents/id.saru.wukongd.plist`
