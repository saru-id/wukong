#!/bin/bash
# Live drill: settings governance against the real daemon, with a fake
# preferences directory. Writes plists the way macOS does (binary, via
# python's plistlib), drives changes through the watcher, and applies
# the manifest back with `wukong settings sync` in file-domain mode.
set -u
cd "$(dirname "$0")/.." || exit 1
cargo build -q -p wukong -p wukongd || exit 1
BIN="${CARGO_TARGET_DIR:-target}/debug"
BIN="$(cd "$BIN" && pwd)"

ROOT=$(mktemp -d)
export HOME="$ROOT/home"
export XDG_CONFIG_HOME="$HOME/.config"
export XDG_DATA_HOME="$HOME/.local/share"
export XDG_STATE_HOME="$HOME/.local/state"
PREFS="$ROOT/prefs"
mkdir -p "$XDG_CONFIG_HOME/wukong" "$PREFS"
git init -q --bare -b main "$ROOT/remote.git"

pref() { # domain key pytype pyvalue  — write via plistlib like macOS would
  python3 - "$PREFS" "$1" "$2" "$3" "$4" <<'PY'
import plistlib, sys, os
prefs, domain, key, ptype, raw = sys.argv[1:6]
name = ".GlobalPreferences.plist" if domain == "NSGlobalDomain" else f"{domain}.plist"
path = os.path.join(prefs, name)
data = {}
if os.path.exists(path):
    data = plistlib.load(open(path, "rb"))
data[key] = {"bool": lambda v: v == "true", "int": int, "float": float, "str": str}[ptype](raw)
plistlib.dump(data, open(path, "wb"), fmt=plistlib.FMT_BINARY)
PY
}
pref_read() { # domain key
  python3 - "$PREFS" "$1" "$2" <<'PY'
import plistlib, sys, os
prefs, domain, key = sys.argv[1:4]
name = ".GlobalPreferences.plist" if domain == "NSGlobalDomain" else f"{domain}.plist"
print(plistlib.load(open(os.path.join(prefs, name), "rb")).get(key))
PY
}

cat > "$XDG_CONFIG_HOME/wukong/config.toml" <<EOF
machine = "sandbox"
remote = "$ROOT/remote.git"
debounce_secs = 1
push_interval_secs = 3600
sentinels = ["~/.zshrc"]
notifications = false

# Hermetic: this drill exercises one lane; the daemon must not detect
# the REAL machine's package world (or fork npm at startup).
[packages]
enabled = false

[settings]
enabled = true
preferences_dir = "$PREFS"
EOF

DB="$XDG_DATA_HOME/wukong/wukong.db"
STORE="$XDG_DATA_HOME/wukong/store"
MANIFEST="$STORE/__wukong__/settings.toml"
W="$BIN/wukong"
pass=0; fail=0
ok()   { pass=$((pass+1)); echo "  PASS  $1"; }
bad()  { fail=$((fail+1)); echo "  FAIL  $1"; }
check(){ if eval "$2"; then ok "$1"; else bad "$1"; fi }
inbox_count(){ sqlite3 "$DB" "SELECT COUNT(*) FROM inbox WHERE resolved=0"; }

# A tuned Mac exists BEFORE the daemon starts.
pref com.apple.dock autohide bool true
pref NSGlobalDomain KeyRepeat int 2

"$BIN/wukongd" > "$ROOT/daemon.log" 2>&1 &
DPID=$!
sleep 1.5

echo "=== baseline"
check "pre-tuned settings baselined silently" "[ \"$(inbox_count)\" = 0 ]"

echo "=== change detected via watcher"
pref com.apple.dock autohide bool false
sleep 3
check "changed setting offered once" "[ \"\$(inbox_count)\" = 1 ]"
check "offer names the setting" "sqlite3 '$DB' \"SELECT subject FROM inbox WHERE resolved=0\" | grep -q 'com.apple.dock autohide'"

echo "=== approve records into the manifest"
ID=$(sqlite3 "$DB" "SELECT id FROM inbox WHERE resolved=0 LIMIT 1")
"$W" resolve "$ID" approve > /dev/null
check "manifest holds the recorded value" "grep -q 'autohide' '$MANIFEST'"
check "committed under the settings banner" "git -C '$STORE' log --oneline | grep -q 'settings: com.apple.dock autohide'"

echo "=== manifest-matching change stays silent"
pref com.apple.dock autohide bool false
sleep 3
check "no offer when live equals desired" "[ \"$(inbox_count)\" = 0 ]"

echo "=== ignore is permanent"
pref NSGlobalDomain KeyRepeat int 6
sleep 3
ID=$(sqlite3 "$DB" "SELECT id FROM inbox WHERE resolved=0 LIMIT 1")
"$W" resolve "$ID" never > /dev/null
pref NSGlobalDomain KeyRepeat int 4
sleep 3
check "ignored key never re-offers" "[ \"$(inbox_count)\" = 0 ]"
check "ignore recorded in manifest" "grep -q 'KeyRepeat' '$MANIFEST'"

echo "=== record outside the corpus, then drift + sync"
pref org.custom.tool fancyMode str on
"$W" settings record org.custom.tool fancyMode > /dev/null
check "arbitrary key recorded" "grep -q 'fancyMode' '$MANIFEST'"
pref org.custom.tool fancyMode str off
pref com.apple.dock autohide bool true
sleep 3
DIFFOUT=$("$W" settings diff)
check "diff shows both drifts" "echo \"\$DIFFOUT\" | grep -q fancyMode && echo \"\$DIFFOUT\" | grep -q autohide"
"$W" settings sync --yes > /dev/null
check "sync applied the recorded values" "[ \"\$(pref_read org.custom.tool fancyMode)\" = on ] && [ \"\$(pref_read com.apple.dock autohide)\" = False ]"
sleep 3
check "post-sync reconcile is silent" "[ \"$(inbox_count)\" = 0 ]"
check "list reports everything in sync" "\"$W\" settings diff | grep -q 'matches this machine'"

echo "=== capture: snapshot, change, diff, record"
"$W" settings capture --start > /dev/null
pref com.apple.screencapture type str png
pref org.some.app "NSWindow Frame main" str "0 0 100 100"
CAPF="$ROOT/cap.json"
"$W" settings capture --diff --json > "$CAPF"
check "capture sees the real change" "grep -q '\"key\": \"type\"' '$CAPF' && grep -q '\"after\": \"png\"' '$CAPF'"
check "corpus label rides along" "grep -q 'screenshots' '$CAPF' || grep -q 'Save screenshots' '$CAPF'"
check "noise excluded from signal json" "! grep -q NSWindow '$CAPF'"
"$W" settings record com.apple.screencapture type > /dev/null
check "captured key recorded into manifest" "grep -q 'screencapture' '$MANIFEST'"
check "second diff without start errors" "! \"$W\" settings capture --diff > /dev/null 2>&1"

echo "=== list --json"
check "settings list --json parses" "\"$W\" settings list --json | python3 -c 'import json,sys; d=json.load(sys.stdin); assert isinstance(d,list) and len(d)>=88'"

echo "=== manifest syncs through the store"
"$W" push > /dev/null
git clone -q -b sandbox "$ROOT/remote.git" "$ROOT/verify"
check "remote carries the settings manifest" "grep -q 'fancyMode' '$ROOT/verify/__wukong__/settings.toml'"

kill $DPID 2>/dev/null
echo
echo "RESULTS: $pass passed, $fail failed"
if [ $fail -eq 0 ]; then echo "DRILL CLEAN"; else echo "daemon log:"; tail -20 "$ROOT/daemon.log"; exit 1; fi
