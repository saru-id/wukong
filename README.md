# wukong

Your Mac's governor. A low-memory daemon and a CLI/TUI that watch the
system so you don't have to remember to. It replaces chezmoi with a
watcher that commits your dotfiles the moment they settle, catches the
side effects an installer sprinkles into your shell files, and (soon)
wraps package managers so everything you add is remembered and nothing
drifts unnoticed.

Built for one person, written for anyone: XDG paths throughout, no
hardcoded machine names, and a `wukong init` that works on any Mac.

## What it does today (v0.1 — the dotfiles governor)

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
  `a`/`r`/`i` to resolve); `wukong status` prints the count; a macOS
  notification fires only on new items.

## Layout

- `crates/core` — the domain: config, XDG paths, the mirror store, the
  secret gate, the SQLite event log, the IPC contract. No daemon, no
  socket; fully tested with `cargo nextest`.
- `crates/wukongd` — the daemon: a tokio event loop over an FSEvents
  watcher, debounce and push timers, and a unix-socket server. Runs as
  a launchd LaunchAgent, idle-cheap.
- `crates/wukong` — the CLI and ratatui TUI: `init`, `track`,
  `untrack`, `status`, `files`, `inbox`, `resolve`, `push`, `daemon`,
  `doctor`. Bare `wukong` opens the dashboard.

## Getting started

```sh
cargo build --release
./target/release/wukong init      # config, store repo, launchd agent
wukong track ~/.zshrc             # its changes now commit on their own
wukong                            # the dashboard
```

On a new machine, point `wukong init` at your existing store remote:
it clones the store, branches for the machine, and `wukong restore`
copies every stored file into place and tracks it.

State lives under XDG: config in `~/.config/wukong`, the store repo and
database in `~/.local/share/wukong`, the socket in `~/.local/state`.

## Roadmap

- v0.2 — package governance: `wukong install/rm` wrapping brew and
  friends, passive adoption of anything installed directly, per-provider
  tracking with opt-out.
- v0.3 — settings governance, carried forward from the catalog work.
- Later — the GUI, as a third client of the same daemon socket.

## Development

```sh
cargo nextest run
cargo clippy --workspace --all-targets
```
