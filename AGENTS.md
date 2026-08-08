# AGENTS.md

Instructions for coding agents working in this repo. Read README.md
first; it is accurate.

## What this is

wukong v2: a system-governing service, NOT the old gpui setup app (that
lives at github.com/saru-id/wukong-app and locally at `../wukong-backup`).
A low-memory daemon (`wukongd`) plus a CLI/TUI (`wukong`); a GUI is a
much later phase.

## Ground rules

- Rust edition 2024, workspace lints: `unsafe_code = "forbid"` and
  **clippy pedantic as warn — keep it at zero warnings**. The few
  allows in Cargo.toml and in code each carry a reason; add new ones
  only with the same justification discipline.
- `crates/core` stays free of the daemon and the socket — pure domain,
  tested with `cargo nextest`. The daemon and CLI compose it.
- The **secret gate is load-bearing**: nothing reaches a commit without
  `gate::scan` clearing it. Never add a path that commits file content
  without going through the gate. New credential patterns are welcome;
  weakening the gate is not.
- **Resolutions are sticky, per fingerprint.** Approve/redact store a
  (path, fingerprint, action) allowance the engine applies on every
  scan. The redacted store copy is re-scanned before commit and held if
  anything unexpected survives — keep that verification step.
- **Inbox bodies are masked evidence.** Diffs and excerpts pass through
  `gate::mask_all` before hitting SQLite. Never store raw file content
  or an unmasked diff in the inbox. Three review-won specifics: a
  finding's excerpt masks EVERY span on its line (never just its own —
  line-mates leak); `mask_all` ignores `wukong:allow` markers (the
  marker exempts commits, never evidence); and detection anchors must
  tolerate diff prefixes (`+KEY=…` — the left boundary is
  "not-a-word-character", never a whitespace whitelist).
- **Binary files** (NUL in the first 8KB) pass the content gate — line
  rules over lossy text would spray entropy false positives. The
  forbidden-name layer still applies.
- The engine (`wukongd/src/engine/`) is the single owner of all
  mutable state — `mod.rs` for the file flow, `packages.rs` for the
  reconcile machine, `tests.rs` for the tempdir suite. One writer, no
  shared locks. Keep it that way.
- The daemon loop speaks only typed `Request`s; JSON parsing and
  encoding live at the edge in the client tasks. Signals become a
  `Msg::Shutdown` like everything else — no `process::exit` inside
  spawned tasks.
- Non-fatal failures go through `soft()` (log to stderr, keep
  governing) — never bare `let _ =` on a fallible governor operation.
  Event/inbox kinds are real enums (`EventKind`, `InboxKind`); strings
  exist only at the database boundary.
- Conventional Commits. Leave work uncommitted unless asked.
- Never use npm/yarn; there is no JS here.

## Hard-won gotchas

- **macOS path canonicalization.** FSEvents reports real paths
  (`/private/var/…`); `$HOME` is often the symlink form (`/var/…`).
  `paths::home()` is canonicalized once for exactly this reason, and
  `paths::resolve_input` canonicalizes CLI input. If tracked files
  stop matching their events, this is why. `data_dir()` is canonical
  AND pinned once at first use (it participates in path comparisons);
  the state/config dirs deliberately are NOT — canonicalizing the
  socket path changes its spelling mid-process and can blow the
  104-byte `sun_path` limit in temp sandboxes. That exact bug shipped
  briefly; the live drills caught it.
- **Store churn must be ignored.** The watcher sees the store's own
  `.git` writes; `touch` filters anything under the store dir and any
  path with a `.git` component. Do not remove that filter.
- **Watch roots are dynamic and almost always non-recursive.** File
  sentinels and tracked files watch their PARENT dir non-recursively
  (survives atomic renames and not-yet-created files); only deliberate
  directory sentinels (~/.config) watch recursively. Never let a
  missing sentinel escalate to a recursive watch of $HOME — that was a
  real bug. The engine accumulates `watch_requests`; main drains them
  after each client request.
- **The hot path stays in memory.** `touch` consults the in-memory
  roster and precomputed sentinel lists — no SQLite per event. Keep it
  that way; a cargo build under a watched root fires thousands of
  events per second.
- **Push runs off-loop** on spawn_blocking with a hard 120s timeout;
  the engine only tracks `push_in_flight`. Nothing else that blocks on
  the network belongs in the event loop. Dirtiness is derived from
  git, not trusted to a bool: startup seeds it from `store.unpushed`,
  and `finish_push` clears it only when the commit counter matches the
  `begin_push` snapshot — a commit landing mid-push stays dirty.
- **The manifest defends itself.** A manifest that exists but fails to
  parse makes saves REFUSE (never save an empty default over the real
  one), and every manifest commit passes `gate::scan` first.
- **One daemon.** Startup connects to the socket first and exits if
  someone answers. Don't remove that guard: launchd KeepAlive plus a
  manual start would otherwise double-commit.
- **Packages reconcile as a set, on transitions only.** `pkg_state` in
  the DB is the last acknowledged reality; offers fire only when it
  and the filesystem disagree (new on-request install not in
  manifest/ignore, or manifest member gone). The first reconcile ever
  baselines silently behind an explicit `__meta__` marker row — a
  pre-wukong machine must not open fifty inbox items. Detection reads
  Cellar receipts (`installed_on_request`), Caskroom dirs, and .app
  bundles — never shell out to brew in the daemon.
- **The manifest is store state, not a live file.** It lives at
  `__wukong__/packages.toml` inside the store repo; `restore` must
  skip the `__wukong__` namespace. For package inbox items, `ignore`
  is PERMANENT (manifest ignore list); redact is invalid. The CLI runs
  brew client-side and reports via PkgRecord — which also supersedes
  any pending offer for the same package.

## Verification

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets
cargo nextest run
```

For the daemon, run the isolated live drill: point `HOME` and the three
`XDG_*` vars at a tempdir, write a `config.toml` with a bare-repo
remote, start `wukongd`, then exercise the full loop: track → edit →
auto-commit (summary must be `+N lines`, not `updated`), a
`FOO_TOKEN=`-shaped secret and a 64-char hex secret (both must
quarantine; the sqlite inbox body must NOT contain the raw token),
approve → next edit must NOT re-quarantine, redact → store masked +
live untouched + sticky, an untracked sentinel change (must be
offered), a `credentials.json` under a sentinel dir (must NOT be
offered), `wukong push` (reply must reflect the real result; the
redacted secret must never appear in `git log -p` on the remote), and
`wukong restore`. Never run the daemon against the real `$HOME` while
testing.

## Runtime paths

- Config: `~/.config/wukong/config.toml`
- Store repo + database: `~/.local/share/wukong/{store,wukong.db}`
- Socket + logs: `~/.local/state/wukong/`
- LaunchAgent: `~/Library/LaunchAgents/id.saru.wukongd.plist`
