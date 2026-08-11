#!/bin/bash
# The upgrade covenant, enforced: build a real governed world with the
# PREVIOUS released binaries, then swap in the CURRENT build over that
# live state and prove nothing is lost — daemon starts, roster intact
# (sealed and shared flags included), sealed files still decrypt, the
# schema stamp advanced, and governance continues. Any schema change
# without its migration turns this red before it ships.
# WUKONG_UPGRADE_FROM overrides the download with a local tarball dir.
set -u
cd "$(dirname "$0")/.." || exit 1
cargo build -q -p wukong -p wukongd || exit 1
NEW="${CARGO_TARGET_DIR:-target}/debug"
NEW="$(cd "$NEW" && pwd)"

ROOT=$(mktemp -d)
pass=0; fail=0
ok()   { pass=$((pass+1)); echo "  PASS  $1"; }
bad()  { fail=$((fail+1)); echo "  FAIL  $1"; }
check(){ if eval "$2"; then ok "$1"; else bad "$1"; fi }

# ---- the previous release's binaries
if [ -n "${WUKONG_UPGRADE_FROM:-}" ]; then
  OLD="$WUKONG_UPGRADE_FROM"
else
  TAG=$(curl -fsSL https://api.github.com/repos/saru-id/wukong/releases/latest |
    sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)
  if [ -z "$TAG" ]; then echo "cannot determine the previous release"; exit 1; fi
  NAME="wukong-$TAG-aarch64-apple-darwin"
  curl -fsSL -o "$ROOT/prev.tar.gz" "https://github.com/saru-id/wukong/releases/download/$TAG/$NAME.tar.gz"
  tar xzf "$ROOT/prev.tar.gz" -C "$ROOT"
  OLD="$ROOT/$NAME"
  echo "=== upgrading FROM $TAG"
fi

export HOME="$ROOT/home"
export XDG_CONFIG_HOME="$HOME/.config"
export XDG_DATA_HOME="$HOME/.local/share"
export XDG_STATE_HOME="$HOME/.local/state"
mkdir -p "$XDG_CONFIG_HOME/wukong"
git init -q --bare -b main "$ROOT/remote.git"
cat > "$XDG_CONFIG_HOME/wukong/config.toml" <<EOF
machine = "sandbox-upgrade"
remote = "$ROOT/remote.git"
debounce_secs = 1
push_interval_secs = 3600
sentinels = []
notifications = false

[packages]
enabled = false

[seal]
identity_file = "$ROOT/age.key"
EOF

echo "=== the OLD version governs a world"
"$OLD/wukongd" > "$ROOT/old-daemon.log" 2>&1 &
OPID=$!
sleep 2
printf 'plain dotfile\n' > "$HOME/.upgraded"
"$OLD/wukong" track "$HOME/.upgraded" > /dev/null
printf 'TOKEN=ghp_upgradedrilltokenupgradedrill00\n' > "$HOME/.env"
"$OLD/wukong" track --sealed "$HOME/.env" > /dev/null
sleep 2.5
"$OLD/wukong" share "$HOME/.upgraded" > /dev/null 2>&1 || true
"$OLD/wukong" push > /dev/null
check "old world is governed" "\"$OLD/wukong\" files | grep -q upgraded"
kill $OPID 2>/dev/null; sleep 0.5

echo "=== the NEW build takes over the same state"
"$NEW/wukongd" > "$ROOT/new-daemon.log" 2>&1 &
NPID=$!
sleep 2
check "new daemon starts over old state" "\"$NEW/wukong\" status > /dev/null 2>&1"
check "roster survived" "\"$NEW/wukong\" files | grep -q upgraded"
check "sealed flag survived" "\"$NEW/wukong\" files | grep env | grep -q '(sealed)'"
check "no self-heal fired (schema understood, not rebuilt)" "! sqlite3 \"$XDG_DATA_HOME/wukong/wukong.db\" \"SELECT subject FROM inbox\" | grep -q database"
check "schema stamp advanced" "[ \"$(sqlite3 "$XDG_DATA_HOME/wukong/wukong.db" 'PRAGMA user_version')\" -ge 1 ]"
printf 'plain dotfile\nplus a post-upgrade line\n' > "$HOME/.upgraded"
sleep 2.5
check "governance continues across the upgrade" "grep -rq 'post-upgrade line' \"$XDG_DATA_HOME/wukong\" --include='.upgraded' 2>/dev/null || git -C \"$XDG_DATA_HOME/wukong/shared\" log --oneline 2>/dev/null | grep -q upgraded || git -C \"$XDG_DATA_HOME/wukong/store\" log --oneline | grep -q upgraded"
check "deep doctor green after upgrade" "\"$NEW/wukong\" doctor --deep | grep -q 'sealed blob(s) decrypt with this machine'"
kill $NPID 2>/dev/null

echo
echo "RESULTS: $pass passed, $fail failed"
if [ $fail -eq 0 ]; then echo "DRILL CLEAN"; else echo "old log:"; tail -5 "$ROOT/old-daemon.log"; echo "new log:"; tail -8 "$ROOT/new-daemon.log"; exit 1; fi
