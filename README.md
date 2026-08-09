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

### Packages (v0.2)

- **Install through wukong, never lose track.** `wukong install jq`
  runs brew (its output streams through untouched) and records the
  package in a manifest that lives in the store — committed, pushed,
  and historied like every dotfile. `wukong rm` is the reverse.
  `--no-track` opts a single install out.
- **Nothing escapes notice.** The daemon watches the brew Cellar and
  Caskroom and /Applications. Install something directly — `brew
  install`, a downloaded .app — and it lands in the inbox as "adopt
  it?". Approve to remember it; ignore to never be asked about that
  package again. Dependencies never surface: detection reads brew's
  own receipts and only what you asked for counts.
- **Symmetry on the way out.** Uninstall a manifest package behind
  wukong's back and the inbox asks whether to drop it or keep it for
  reinstall.
- **The new-machine answer.** `wukong pkg sync` installs everything in
  the manifest that's missing (and prints the checklist of apps it can
  remember but not install). `wukong pkg adopt-installed` bulk-imports
  an existing machine's brew world on day one.

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

- v0.3 — settings governance, carried forward from the catalog work;
  more package providers (mas, npm/cargo/pipx globals).
- Later — the GUI, as a third client of the same daemon socket.

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
