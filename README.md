# wukong

Your Mac's governor. A low-memory daemon and a CLI/TUI that watch the
system so you don't have to remember to. It replaces chezmoi with a
watcher that commits your dotfiles the moment they settle, catches the
side effects an installer sprinkles into your shell files, and (soon)
wraps package managers so everything you add is remembered and nothing
drifts unnoticed.

Built for one person, written for anyone: XDG paths throughout, no
hardcoded machine names, and a `wukong init` that works on any Mac.

## What it does today

- **Never forget to commit.** Track a file; every settled change
  commits to a private mirror repo on this machine's branch, with a
  generated message, and pushes on a timer.
- **The secret gate.** No change reaches a commit without passing a
  built-in scanner (a curated set of credential shapes plus a
  charset-aware entropy check that catches hex and base64 tokens too)
  and a denylist of files that are never trackable (private keys,
  `.env`). A hit is held in the review inbox, never in git — and the
  evidence stored with it is masked, so the database never holds a raw
  secret either. The gate cannot be disabled globally, only per
  finding.
- **The sealed lane.** Some files are secrets — `.env`, `.netrc`, an
  API config. Track them `--sealed` (or resolve a quarantine with
  `seal`) and the store holds only age ciphertext: the remote never
  sees their plaintext, period. The private identity lives in your
  Keychain; the public recipient syncs with the store so every machine
  can encrypt; `wukong seal-key export`/`import` moves the one secret
  that matters between machines, through a channel you trust.
- **Resolutions stick.** Every finding has a fingerprint of the secret
  itself. Approve it once and that token commits forever without
  another prompt; redact it once and every future stored copy masks it
  automatically (the live file is never touched). Rotate the token and
  the new value quarantines fresh, exactly as it should.
- **Side-effect discovery.** The daemon watches a sentinel set it does
  not track — your shell startup files, `~/.config`, launchd agents —
  and when an installer edits one, it lands in the inbox as
  "changed — start tracking it?".
- **The inbox, everywhere.** The TUI opens on it (diffs inline,
  `a`/`r`/`i` to resolve, `x` to exclude a noisy subtree for good);
  `wukong status` prints the count; a macOS notification fires only on
  new items.
- **Built to be inspected.** `wukong diff` shows live vs stored,
  `wukong log` shows a file's commit history, `wukong status` says how
  long ago the last push landed — the daemon is auditable from any
  shell, which matters most on a machine you are not sitting at.

### Packages

- **Install through wukong, never lose track.** `wukong install jq`
  runs brew (its output streams through untouched) and records the
  package in a manifest that lives in the store — committed, pushed,
  and historied like every dotfile. `wukong rm` is the reverse.
  `--no-track` opts a single install out, and `--via` picks any of
  the fourteen providers: brew formulae and casks, apps (App Store
  and drag-installed), and global npm, pnpm, bun, cargo, go, gem,
  pipx, uv, dotnet, and pub tools.
- **Nothing escapes notice.** Every provider leaves receipts on
  disk, and the daemon reads receipts instead of asking tools:
  brew's Cellar receipts, `.app` bundles (App Store ones carry a
  `_MASReceipt` and are classified apart), global `node_modules`
  trees, cargo's `.crates.toml`, the module path Go embeds in every
  binary it builds, gemspec files, pipx and uv venvs, dotnet's
  `.store`, pub's `global_packages`. Install something directly —
  `brew install`, `npm i -g`, `go install`, the App Store — and it
  lands in the inbox as "adopt it?". Approve to remember it; ignore
  to never be asked about that package again. Dependencies never
  surface: only what you asked for counts.
- **Auditable roots.** `wukong pkg providers` shows every provider's
  watched root, how it was found (fixed, probed once at startup, or
  a config override), and what it currently sees — the standing
  answer to "why isn't X being offered?". `wukong pkg list` shows
  installed versions straight from the receipts.
- **Symmetry on the way out.** Uninstall a manifest package behind
  wukong's back and the inbox asks whether to drop it or keep it for
  reinstall.
- **The new-machine answer.** `wukong pkg sync` installs everything in
  the manifest that's missing, each package through its own manager —
  it shows the exact commands first, and `--dry-run` stops there.
  App Store apps install by the id wukong captured at adoption;
  anything it can only remember comes back as a checklist. `wukong
  pkg adopt-installed` bulk-imports an existing machine's whole
  package world on day one.

### Settings

- **macOS behavior, governed like everything else.** A curated corpus
  of 88 defaults — Dock, Finder, keyboard, trackpad, screenshots,
  window management — is watched for change. Tweak something in System
  Settings and the inbox offers to record the new value; recorded
  values live in a manifest that commits and syncs like every dotfile.
- **`wukong settings sync`** applies the recorded values on a new
  machine — through `defaults` (never raw plist writes, so `cfprefsd`
  stays coherent) — and restarts Dock/Finder/SystemUIServer exactly
  once each, as the corpus prescribes. `settings diff` shows drift;
  `settings record` governs any domain/key beyond the corpus.
- Ignoring a setting offer is permanent, per key — fiddle with your
  mouse speed forever without being asked about it again.
- **Any setting, discoverable.** `wukong settings capture` snapshots
  every preference key, waits while you change the thing — System
  Settings, an app, `defaults` — then shows you exactly which keys
  changed, app furniture filtered out. Record what you choose, and
  from then on it's governed like a corpus member. The corpus is a
  seed, not a ceiling.

## Layout

- `crates/core` — the domain: config, XDG paths, the mirror store, the
  secret gate, the SQLite event log, the IPC contract. No daemon, no
  socket; fully tested with `cargo nextest`.
- `crates/wukongd` — the daemon: a tokio event loop over an FSEvents
  watcher, debounce and push timers, and a unix-socket server. Runs as
  a launchd LaunchAgent, idle-cheap.
- `crates/wukong` — the CLI and ratatui TUI: `init`, `adopt-dotfiles`,
  `install`, `rm`, `pkg`, `track`, `untrack`, `exclude`, `diff`, `log`,
  `status`, `files`, `inbox`, `resolve`, `push`, `restore`, `daemon`,
  `doctor`. Bare `wukong` opens the dashboard.

## Install from a release

Each release ships `wukong-vX.Y.Z-aarch64-apple-darwin.tar.gz`
containing the binaries, man pages, and zsh completions:

```sh
tar xzf wukong-v*.tar.gz && cd wukong-v*/
install -m 755 wukong wukongd ~/.local/bin/
cp share/man/man1/*.1 /usr/local/share/man/man1/ 2>/dev/null || true
cp share/zsh/site-functions/_wukong ~/.zsh/completions/ 2>/dev/null || true
```

Then `wukong init`, and the bootstrap trio: `wukong restore` for
files, `wukong pkg sync` for software, `wukong settings sync` for
behavior. Leaving is as clean as arriving:
`wukong uninstall` stops the daemon and removes the agent (data kept);
`--purge` removes local data too. The remote store is never touched.

## Getting started

```sh
cargo build --release             # or grab the release tarball
./target/release/wukong init      # config, store repo, launchd agent
wukong adopt-dotfiles             # find + track this machine's dotfiles
wukong                            # the dashboard
```

On a new machine, point `wukong init` at your existing store remote:
it clones the store, branches for the machine, and `wukong restore`
copies every stored file into place and tracks it.

State lives under XDG: config in `~/.config/wukong`, the store repo and
database in `~/.local/share/wukong`, the socket in `~/.local/state`.

## Roadmap

- pip stays out on purpose: PEP 668 made pip refuse global installs
  on modern systems, and pipx/uv are its governed successors here.
- The GUI, as a third client of the same daemon socket.

## Development

```sh
make check   # fmt, clippy (pedantic, warnings-as-errors), tests
make drill   # live drills: the real daemon in a sandbox
```

CI runs the same on every push — plus the RustSec advisory scan
(weekly too), the property tests, and both live drills. Releases get
an arm64 tarball attached automatically on publish, complete with man
pages (`man wukong`, `man wukong-resolve`, …) and zsh completions;
`wukong <command> --help` explains every verb's exact semantics.
