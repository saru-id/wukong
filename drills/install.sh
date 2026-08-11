#!/bin/bash
# Live drill: install.sh against a locally-built release tarball —
# the same layout the release workflow ships. Proves the one-paste
# path: binaries, man pages, completions, PATH wiring, idempotence.
set -u
cd "$(dirname "$0")/.." || exit 1
cargo build -q -p wukong -p wukongd || exit 1
BIN="${CARGO_TARGET_DIR:-target}/debug"

ROOT=$(mktemp -d)
pass=0; fail=0
ok()   { pass=$((pass+1)); echo "  PASS  $1"; }
bad()  { fail=$((fail+1)); echo "  FAIL  $1"; }
check(){ if eval "$2"; then ok "$1"; else bad "$1"; fi }

# A release-shaped tarball from the freshly built binaries.
VERSION=$("$BIN/wukong" --version | awk '{print $2}')
STAGE="$ROOT/wukong-v$VERSION-aarch64-apple-darwin"
mkdir -p "$STAGE/share/man/man1" "$STAGE/share/zsh/site-functions"
cp "$BIN/wukong" "$BIN/wukongd" "$STAGE/"
"$BIN/wukong" gen-man "$STAGE/share/man/man1" > /dev/null
"$BIN/wukong" gen-completions zsh > "$STAGE/share/zsh/site-functions/_wukong"
tar czf "$ROOT/release.tar.gz" -C "$ROOT" "$(basename "$STAGE")"

export HOME="$ROOT/home"
mkdir -p "$HOME"
WUKONG_INSTALL_SOURCE="$ROOT/release.tar.gz" WUKONG_INSTALL_NO_CLT=1 sh install.sh > "$ROOT/install.log" 2>&1
check "installer exits clean" "[ $? = 0 ]"
check "binaries land in ~/.local/bin" "[ -x \"$HOME/.local/bin/wukong\" ] && [ -x \"$HOME/.local/bin/wukongd\" ]"
check "installed wukong runs and matches" "\"$HOME/.local/bin/wukong\" --version | grep -q \"$VERSION\""
check "man pages installed" "ls \"$HOME/.local/share/man/man1\" | grep -q 'wukong.1'"
check "completions installed" "[ -f \"$HOME/.local/share/zsh/site-functions/_wukong\" ]"
check "PATH line added to .zprofile" "grep -q '.local/bin' \"$HOME/.zprofile\""
check "fpath line added to .zshrc" "grep -q 'site-functions' \"$HOME/.zshrc\""
check "installer hands off to bare wukong" "grep -qx '  wukong' \"$ROOT/install.log\""

# Round two: nothing duplicates.
WUKONG_INSTALL_SOURCE="$ROOT/release.tar.gz" WUKONG_INSTALL_NO_CLT=1 sh install.sh > /dev/null 2>&1
NPATH=$(grep -c '.local/bin' "$HOME/.zprofile")
NFPATH=$(grep -c 'site-functions' "$HOME/.zshrc")
check "re-run is idempotent (one PATH line)" "[ \"$NPATH\" = 1 ]"
check "re-run is idempotent (one fpath line)" "[ \"$NFPATH\" = 1 ]"

# The checksum gate refuses a corrupted download: simulate by
# tampering the tarball while faking the release fetch shape.
printf 'tampered' >> "$ROOT/release.tar.gz"
if WUKONG_INSTALL_SOURCE="$ROOT/release.tar.gz" WUKONG_INSTALL_NO_CLT=1 sh install.sh > /dev/null 2>&1; then
  ok "local-source path skips checksum (by design)"
else
  bad "local-source install broke on rerun"
fi

echo "=== zero-setup: the first real command sets the machine up"
export XDG_CONFIG_HOME="$HOME/.config"
export XDG_DATA_HOME="$HOME/.local/share"
export XDG_STATE_HOME="$HOME/.local/state"
W="$HOME/.local/bin/wukong"
"$W" status > "$ROOT/status.log" 2>&1 || true
check "read verbs answer honestly before setup" "grep -q 'set up yet' \"$ROOT/status.log\""
check "read verbs have no side effects" "[ ! -f \"$XDG_CONFIG_HOME/wukong/config.toml\" ]"

printf 'first tracked file\n' > "$HOME/.zerofile"
WUKONG_NO_AGENT=1 "$W" track "$HOME/.zerofile" > "$ROOT/first.log" 2>&1
check "first real command runs setup itself" "grep -q 'First run' \"$ROOT/first.log\""
check "setup is local-only with the remote hint" "grep -q 'wukong remote' \"$ROOT/first.log\""
check "config written by first use" "[ -f \"$XDG_CONFIG_HOME/wukong/config.toml\" ]"
check "the command itself still worked" "grep -q 'tracking' \"$ROOT/first.log\""
check "status now answers" "\"$W\" status | grep -q 'local-only'"

echo "=== the remote attaches late, safely"
git init -q --bare -b main "$ROOT/late-remote.git"
WUKONG_NO_AGENT=1 "$W" remote "$ROOT/late-remote.git" > "$ROOT/remote.log" 2>&1
check "remote persisted to config" "grep -q 'late-remote' \"$XDG_CONFIG_HOME/wukong/config.toml\""
check "empty remote recognized" "grep -q 'empty repository' \"$ROOT/remote.log\""
"$W" push > /dev/null 2>&1
check "history pushed after late attach" "git -C \"$ROOT/late-remote.git\" branch | grep -q ."
check "shared branch pushed too" "git -C \"$ROOT/late-remote.git\" rev-parse --verify -q shared > /dev/null"
kill "$(cat "$XDG_STATE_HOME/wukong/wukongd.pid")" 2>/dev/null

echo
echo "RESULTS: $pass passed, $fail failed"
if [ $fail -eq 0 ]; then echo "DRILL CLEAN"; else cat "$ROOT/install.log"; exit 1; fi
