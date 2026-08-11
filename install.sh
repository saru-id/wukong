#!/bin/sh
# The wukong installer: a clean Mac to ready-for-`wukong init`, one
# paste, no prerequisites — not even git (it triggers Apple's Command
# Line Tools install and waits).
#
#   curl -fsSL https://raw.githubusercontent.com/saru-id/wukong/main/install.sh | sh
#
# Everything lands under ~/.local (override: WUKONG_INSTALL_PREFIX).
# Drills install from a local tarball via WUKONG_INSTALL_SOURCE and
# skip the CLT check via WUKONG_INSTALL_NO_CLT.
set -eu

main() {
  REPO="saru-id/wukong"
  PREFIX="${WUKONG_INSTALL_PREFIX:-$HOME/.local}"

  [ "$(uname -s)" = "Darwin" ] || fail "wukong runs on macOS only"
  [ "$(uname -m)" = "arm64" ] || fail "wukong releases are Apple-silicon (arm64) only"

  # git ships with the Command Line Tools; wukong shells to it for
  # everything. Kick off the system installer and wait it out.
  if [ -z "${WUKONG_INSTALL_NO_CLT:-}" ] && ! xcode-select -p > /dev/null 2>&1; then
    say "wukong needs git (Apple's Command Line Tools) — starting the system installer…"
    xcode-select --install > /dev/null 2>&1 || true
    say "waiting for the Command Line Tools (click through the dialog; this can take a few minutes)…"
    until xcode-select -p > /dev/null 2>&1; do sleep 5; done
    say "✓ Command Line Tools installed"
  fi

  WORK=$(mktemp -d)
  trap 'rm -rf "$WORK"' EXIT

  if [ -n "${WUKONG_INSTALL_SOURCE:-}" ]; then
    TARBALL="$WUKONG_INSTALL_SOURCE"
    say "installing from $TARBALL"
  else
    say "fetching the latest release…"
    TAG=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" |
      sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)
    [ -n "$TAG" ] || fail "could not determine the latest release"
    NAME="wukong-$TAG-aarch64-apple-darwin"
    BASE="https://github.com/$REPO/releases/download/$TAG"
    curl -fsSL -o "$WORK/$NAME.tar.gz" "$BASE/$NAME.tar.gz"
    curl -fsSL -o "$WORK/$NAME.tar.gz.sha256" "$BASE/$NAME.tar.gz.sha256"
    (cd "$WORK" && shasum -a 256 -c "$NAME.tar.gz.sha256" > /dev/null) ||
      fail "checksum verification FAILED — refusing to install"
    say "✓ $TAG downloaded and verified"
    TARBALL="$WORK/$NAME.tar.gz"
  fi

  tar xzf "$TARBALL" -C "$WORK"
  STAGE=$(find "$WORK" -maxdepth 1 -type d -name 'wukong-*' | head -1)
  [ -n "$STAGE" ] || fail "tarball layout not recognized"

  mkdir -p "$PREFIX/bin" "$PREFIX/share/man/man1" "$PREFIX/share/zsh/site-functions"
  install -m 755 "$STAGE/wukong" "$STAGE/wukongd" "$PREFIX/bin/"
  cp "$STAGE"/share/man/man1/*.1 "$PREFIX/share/man/man1/" 2> /dev/null || true
  cp "$STAGE"/share/zsh/site-functions/_wukong "$PREFIX/share/zsh/site-functions/" 2> /dev/null || true
  say "✓ installed to $PREFIX/bin (man pages and completions beside it)"

  # A fresh Mac's PATH has no ~/.local/bin; add it exactly once. man
  # finds $PREFIX/share/man on its own once bin is on PATH.
  # shellcheck disable=SC2016 # literal $HOME/$fpath is the point
  if [ "$PREFIX" = "$HOME/.local" ]; then
    if ! grep -qs '\.local/bin' "$HOME/.zprofile"; then
      printf '\n# wukong installer: user binaries\nexport PATH="$HOME/.local/bin:$PATH"\n' >> "$HOME/.zprofile"
      say "✓ added ~/.local/bin to PATH (~/.zprofile)"
    fi
    if ! grep -qs 'zsh/site-functions' "$HOME/.zshrc" 2> /dev/null; then
      printf '\n# wukong installer: completions\nfpath=("$HOME/.local/share/zsh/site-functions" $fpath)\n' >> "$HOME/.zshrc"
      say "✓ added completions to fpath (~/.zshrc)"
    fi
  fi

  say ""
  say "Done. One command sets the whole machine up:"
  say ""
  say "  wukong init"
  say ""
  say "(open a new terminal first, or: export PATH=\"\$HOME/.local/bin:\$PATH\")"
}

say() { printf '%s\n' "$1"; }
fail() {
  printf 'install.sh: %s\n' "$1" >&2
  exit 1
}

main "$@"
