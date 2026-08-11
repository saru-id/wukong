# AGENTS.md

Instructions for coding agents working in this repo. Read README.md
first; it is accurate.

## What this is

wukong is a system-governing service: a low-memory daemon (`wukongd`)
plus a CLI/TUI (`wukong`); a GUI comes later as a third client. Do not
confuse this repo with `saru-id/wukong-app` (a separate, unrelated gpui
application that may sit nearby on disk as `../wukong-backup`).

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
- **The sealed lane never touches plaintext in the store.** Sealed
  commits are gated on a plaintext SHA-256 (age is non-deterministic —
  without the hash guard, unchanged files would commit forever). The
  identity is NEVER in the store; the recipient always is. Sealed
  tracking is the only path that may bypass the forbidden-name refusal
  — because ciphertext-only storage is exactly what makes those names
  safe. Unseal goes back through the gate and may quarantine.
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
- **Help is part of the feature.** Every verb carries an `about` and a
  `long_about` that states its exact semantics (what approve/ignore
  MEAN, what is permanent, what is never touched); man pages and
  completions are generated from the same clap definitions (hidden
  gen-man / gen-completions commands), so the CLI is the single source
  of documentation truth.
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
  missing sentinel escalate to a recursive watch of $HOME — its parent
  IS $HOME, and recursion there tails every build tree the user owns.
  The engine accumulates `watch_requests`; main drains them after
  EVERY loop message (promotions and re-detections fire from
  non-client messages).
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
- **Excludes are a live verb, not a config chore.** `wukong exclude`
  (and `x` in the TUI) applies in memory, persists via the config's
  `source` path, and resolves open offers under the prefix. A config
  built in memory (tests) has `source: None` and never writes to the
  real user's file — keep it that way.
- **One daemon.** Startup connects to the socket first and exits if
  someone answers. Don't remove that guard: launchd KeepAlive plus a
  manual start would otherwise double-commit.
- **Packages reconcile as a set, on transitions only.** `pkg_state` in
  the DB is the last acknowledged reality; offers fire only when it
  and the filesystem disagree (new on-request install not in
  manifest/ignore, or manifest member gone). The first reconcile ever
  baselines silently behind an explicit `__meta__` marker row — a
  pre-wukong machine must not open fifty inbox items. Detection reads
  each manager's own on-disk receipts: Cellar receipts
  (`installed_on_request`), Caskroom dirs, .app bundles (an
  `_MASReceipt` classifies an app as `mas`, the rest are `app`),
  global `node_modules` trees (npm/pnpm/bun, `@scope/name` expanded,
  `.bin` and hidden entries skipped), `~/.cargo/.crates.toml`, the
  buildinfo blob inside every go binary (core/src/gobuild.rs, cached
  by mtime+size — never re-read an unchanged binary), gemspec file
  names, pipx/uv venvs, dotnet's `.store`, pub's `global_packages`.
- **Provider forks happen on two occasions only.** (1) At startup or
  on the rescan heartbeat (`redetect_roots`), to LOCATE a root
  (`npm root -g`, `pnpm root -g`); (2) on an explicit user action, to
  enrich it (`mdls` fetching an App Store id when the user approves a
  mas adoption). NEVER on the reconcile/watch path, and never to LIST
  packages — reconcile stays pure file reads. A `[packages.roots]`
  override suppresses the locate fork (defaults are lazy); an
  override at a nonexistent dir disables the provider. Engine tests
  MUST override every language provider to absent paths (see
  `pkg_rig`) or the developer's real machine leaks into the suite.
- **Settings: read plists, write `defaults`, never the reverse.**
  Reads go straight to the preference plists (fast, forkless, the
  `plist` crate); writes MUST go through the `defaults` CLI or
  `cfprefsd` will fight you. The reconcile is transition-based against
  `settings_state` with a `__meta__` baseline row; a change that
  matches the manifest's desired value acknowledges silently AND
  auto-resolves any stale offer. Bool/Int coercion and float epsilon
  live in `settings::Value::matches` — compare with it, never with
  `==`. Complex values (`Value::Complex { plist }`, XML) compare
  STRUCTURALLY via parsed plists; they enter only through explicit
  `settings record` (from_plist_any on the governed read path) —
  capture and ambient discovery stay scalar (from_plist), or arrays
  of app-state noise would drown the inbox. Apply passes plist text
  to `defaults write`, no type flag. The corpus (crates/core/src/settings.rs) carries label and
  restart knowledge only; desired values live in the manifest.
- **Capture is bounded and one-shot.** The snapshot lives only in
  daemon memory, expires after 10 minutes, and is consumed by the
  diff. The noise filter (settings.rs NOISE_*) is curated and tested
  against the whole corpus — every corpus key must classify as signal;
  when adding markers, anchor on the chaff form ("NSToolbar
  Configuration", not "NSToolbar") or real settings get eaten.
- **The manifest is store state, not a live file.** It lives at
  `__wukong__/packages.toml` inside the store repo; `restore` must
  skip the `__wukong__` namespace. For package inbox items, `ignore`
  is PERMANENT (manifest ignore list); redact is invalid. The CLI runs
  brew client-side and reports via PkgRecord — which also supersedes
  any pending offer for the same package.

- **One vocabulary for inbox decisions: approve / never / skip.**
  `never` is ALWAYS the permanent opt-out (sentinel: exclude the path;
  package/setting: manifest ignore list; quarantine: INVALID — a
  secret can't be waved off forever). `skip` is ALWAYS harmless: close
  the item, promise nothing (a quarantined change stays held out of
  git). Quarantines add `redact`/`seal`. Never reintroduce a
  resolution whose blast radius depends on the item kind — that
  ambiguity is exactly what this vocabulary replaced.
- **Setup is not a step.** `init::ensure_ready` runs from `preflight`
  on the first REAL command (and bare `wukong`, which adds the
  one-time welcome): config local-only, store, daemon, zero
  questions. Read-shaped verbs (status/doctor/files/lists) must NEVER
  set up as a side effect — they answer "not set up yet" honestly.
  The remote is late-bindable via `wukong remote` (probe the remote
  BEFORE bouncing the daemon — it may push the instant it restarts);
  attach-later is safe because machine branches are per-machine and
  the shared lane folds on the rebase path.
- **`init` is the whole lifecycle; `sync` is the whole convergence.**
  init ends by offering `sync` (store has this machine's world) or
  `adopt` (machine brings a world in) — one command, one confirmation,
  `--yes` for unattended runs. `wukong sync` composes restore (plan
  via `Restore { dry_run }`), the pkg plan, and the settings plan into
  ONE confirm; the scoped verbs stay for à la carte use. Drills that
  exercise one lane MUST disable the others in their config (packages
  detection would otherwise read the developer's real machine).

- **The shared lane is an overlay, and the machine ALWAYS wins.** The
  `shared` branch lives as a sibling worktree (`Store::shared()` is a
  full Store against it — never construct a second Store by path).
  Effective wanted/desired = machine ∪ shared with machine winning;
  restore/sync unions the file lists the same way. A NEW machine's
  branch starts EMPTY (orphan root) — seeding it from clone HEAD
  would shadow the shared lane forever. Shared push: try, and on
  rejection fold origin in with `rebase -X theirs` (in a rebase,
  "theirs" is the LOCAL patch — live files win across machines too).
  refresh_shared runs on the rescan heartbeat and must reload the
  shared manifests; live files are never rewritten from remote —
  that's `wukong sync`'s job, on the user's command. Sealed shared
  files require the seal identity on every machine (seal-key
  export/import).

- **Health checks nag boundedly and act conservatively.** At most
  hourly (`health_tick` piggybacks on tick), one alert per subject per
  24h window gated by the Health EVENT's timestamp (works across skip
  and daemon restarts). Health items take approve/skip only; approve
  runs the obvious fix where one exists (push → set dirty, the loop
  pushes) and never anything destructive. `revert` writes the OLD
  content to the LIVE file and lets the normal debounce/gate/commit
  flow pick it up — never a git rewrite, and a reverted-to secret
  still quarantines.

- **Every fork gets a deadline; every test forks nothing real.** Any
  child process the daemon spawns (push 120s, root probes 10s) has a
  hard wall clock — a wedged shim costs its feature, never the
  daemon. And no unit test may fork a real package manager: a hanging
  `pnpm root -g` once froze the suite for two hours. Override npm AND
  pnpm to absent paths in every detect_roots test.

- **The trust model: one domain, made visible.** The remote and every
  machine holding the store are a single trust domain — wukong makes
  cross-machine effects VISIBLE and DELIBERATE rather than defending
  against a hostile member. Concretely: shared arrivals become an
  inbox item naming every file (fold_shared_arrivals; approve applies
  via the conservative restore path), `dangerous_rel` splits BLOCKING
  paths (LaunchAgents/LaunchDaemons/bin — bulk restore refuses;
  single-file --force places) from flag-only (`__abs__` outside-home,
  shown but not blocked: deliberately tracked outside-home files must
  survive recovery without ceremony), and write_private is O_NOFOLLOW.
- **Nothing is ever the only copy.** Overwrites (revert, restore
  --force) archive prior bytes BESIDE the store (`../overwritten` —
  never a global path; sandboxes get sandboxed archives). An empty
  roster with a non-empty store self-heals — but ONLY for files whose
  live copy exists (tracking store-only files would commit phantom
  removals on the next settle); allowances stay lost on purpose
  (fail-closed). The seal escrow (`__wukong__/age.key.enc`, shared
  lane) is passphrase-encrypted CLIENT-side; the daemon only ever
  files ciphertext and refuses anything that isn't.

## Verification

```sh
make check   # fmt + clippy + tests (what CI's check job runs)
make drill   # both live drills, sandboxed
make audit   # RustSec advisory scan
make ci      # all of the above
```

The toolchain is pinned in rust-toolchain.toml so local and CI clippy
always agree; bump it deliberately, in its own commit.

The live drills ARE in the repo: `make drill` runs both
(`drills/dotfiles.sh`, `drills/packages.sh`, `drills/settings.sh`,
`drills/shared.sh`, `drills/dayone.sh`) — the real daemon in a
sandboxed HOME/XDG tempdir, replaying every failure mode past reviews
found. CI runs them on every push. Extend the drills whenever a new
failure mode is fixed; never run the daemon against the real `$HOME`
while testing.

## Runtime paths

- Config: `~/.config/wukong/config.toml`
- Store repo + database: `~/.local/share/wukong/{store,wukong.db}`
- Socket + logs: `~/.local/state/wukong/`
- LaunchAgent: `~/Library/LaunchAgents/id.saru.wukongd.plist`
