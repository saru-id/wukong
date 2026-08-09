#!/bin/bash
# Live drill: the dotfiles governor, adversarially. Runs the REAL
# daemon in an isolated sandbox (HOME + XDG point at a tempdir, a bare
# repo plays the remote) and replays every failure mode past reviews
# found. This is the verification method of record — AGENTS.md law.
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
mkdir -p "$HOME/.config" "$XDG_CONFIG_HOME/wukong"
git init -q --bare -b main "$ROOT/remote.git"

cat > "$XDG_CONFIG_HOME/wukong/config.toml" <<EOF
machine = "sandbox"
remote = "$ROOT/remote.git"
debounce_secs = 1
push_interval_secs = 3600
sentinels = ["~/.zshrc", "~/.zprofile", "~/.config"]
notifications = false
exclude = ["~/.config/wukong"]
EOF

DB="$XDG_DATA_HOME/wukong/wukong.db"
STORE="$XDG_DATA_HOME/wukong/store"
W="$BIN/wukong"
pass=0; fail=0
ok()   { pass=$((pass+1)); echo "  PASS  $1"; }
bad()  { fail=$((fail+1)); echo "  FAIL  $1"; }
check(){ if eval "$2"; then ok "$1"; else bad "$1"; fi }

"$BIN/wukongd" > "$ROOT/daemon.log" 2>&1 &
DPID=$!
sleep 1

echo "=== single instance"
"$BIN/wukongd" > "$ROOT/second.log" 2>&1
check "second daemon refuses to start" "grep -q 'already running' '$ROOT/second.log'"

echo "=== track + clean edit"
echo 'export A=1' > "$HOME/.zshrc"
"$W" track ~/.zshrc > /dev/null
sleep 2.5
printf 'export A=1\nexport B=2\n' > "$HOME/.zshrc"
sleep 2.5
check "clean edit committed" "grep -q 'export B=2' '$STORE/.zshrc'"
check "summary is real, not 'updated'" "git -C '$STORE' log --oneline | grep -q '+1 lines'"

echo "=== v0.1-blind secret shapes must quarantine"
TOKEN=wJalrXUtnFEMIK7MDENGbPxRfiCYEXAMPLEKEY0
printf 'export A=1\nexport B=2\nexport MY_APP_TOKEN=%s\n' "$TOKEN" > "$HOME/.zshrc"
sleep 2.5
check "FOO_TOKEN shape quarantined"   "! grep -q '$TOKEN' '$STORE/.zshrc'"
check "quarantine inbox item exists"  "[ \"\$(sqlite3 '$DB' \"SELECT COUNT(*) FROM inbox WHERE kind='quarantine' AND resolved=0\")\" = 1 ]"
check "inbox body is masked (no raw token in DB)" "! sqlite3 '$DB' 'SELECT body FROM inbox' | grep -q '$TOKEN'"

echo "=== approve is sticky"
ID=$(sqlite3 "$DB" "SELECT id FROM inbox WHERE resolved=0 LIMIT 1")
"$W" resolve "$ID" approve > /dev/null
sleep 2.5
check "approved token committed" "grep -q '$TOKEN' '$STORE/.zshrc'"
printf 'export A=1\nexport B=2\nexport MY_APP_TOKEN=%s\nexport C=3\n' "$TOKEN" > "$HOME/.zshrc"
sleep 2.5
check "same token: NO re-quarantine on next edit" "[ \"\$(sqlite3 '$DB' 'SELECT COUNT(*) FROM inbox WHERE resolved=0')\" = 0 ]"
check "next edit committed through" "grep -q 'export C=3' '$STORE/.zshrc'"

echo "=== redact is sticky and store-only"
HEX=9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08
echo 'export G=1' > "$HOME/.gitconfig-custom"
"$W" track ~/.gitconfig-custom > /dev/null
sleep 1.5
printf 'export G=1\nexport API_HASH=%s\n' "$HEX" > "$HOME/.gitconfig-custom"
sleep 2.5
check "hex secret quarantined (old entropy bar missed it)" "[ \"\$(sqlite3 '$DB' 'SELECT COUNT(*) FROM inbox WHERE resolved=0')\" = 1 ]"
ID=$(sqlite3 "$DB" "SELECT id FROM inbox WHERE resolved=0 LIMIT 1")
"$W" resolve "$ID" redact > /dev/null
sleep 2.5
check "store copy masked"      "! grep -q '$HEX' '$STORE/.gitconfig-custom'"
check "store keeps masked stub" "grep -q '9f86……' '$STORE/.gitconfig-custom'"
check "live file untouched"     "grep -q '$HEX' '$HOME/.gitconfig-custom'"
printf 'export G=1\nexport API_HASH=%s\nexport H=2\n' "$HEX" > "$HOME/.gitconfig-custom"
sleep 2.5
check "redaction sticky on next edit" "! grep -q '$HEX' '$STORE/.gitconfig-custom' && grep -q 'export H=2' '$STORE/.gitconfig-custom'"
check "no re-quarantine after redact" "[ \"\$(sqlite3 '$DB' 'SELECT COUNT(*) FROM inbox WHERE resolved=0')\" = 0 ]"

echo "=== sentinel discovery + forbidden skip"
echo 'eval brew shellenv' > "$HOME/.zprofile"
mkdir -p "$HOME/.config/sometool"
echo '{"token":"supersecret"}' > "$HOME/.config/sometool/credentials.json"
sleep 2.5
check "sentinel .zprofile offered" "sqlite3 '$DB' \"SELECT subject FROM inbox WHERE kind='sentinel' AND resolved=0\" | grep -q zprofile"
check "forbidden credentials.json NOT offered" "! sqlite3 '$DB' 'SELECT subject FROM inbox' | grep -q credentials"

echo "=== push truthfulness + remote hygiene"
OUT=$("$W" push)
sleep 1
check "push reports real result" "echo \"\$OUT\" | grep -q '^pushed$'"
git clone -q -b sandbox "$ROOT/remote.git" "$ROOT/verify"
check "remote has approved token (deliberate)" "grep -q '$TOKEN' '$ROOT/verify/.zshrc'"
check "remote never saw redacted hex"          "! git -C '$ROOT/verify' log -p --all | grep -q '$HEX'"

echo "=== noise valve: wukong exclude"
mkdir -p "$HOME/.config/noisyapp"
echo '{"s":1}' > "$HOME/.config/noisyapp/state.json"
sleep 2.5
noisy(){ sqlite3 "$DB" "SELECT COUNT(*) FROM inbox WHERE resolved=0 AND subject LIKE '%noisyapp%'"; }
check "noisy subtree offer appears" "[ \"\$(noisy)\" = 1 ]"
"$W" exclude ~/.config/noisyapp > /dev/null
check "exclude resolves the open offer" "[ \"\$(noisy)\" = 0 ]"
echo '{"s":2}' > "$HOME/.config/noisyapp/state.json"
sleep 2.5
check "excluded subtree stays silent" "[ \"\$(noisy)\" = 0 ]"
check "exclude persisted to config" "grep -q noisyapp '$XDG_CONFIG_HOME/wukong/config.toml'"

echo "=== adopt-dotfiles"
printf '[user]\n\tname = s\n' > "$HOME/.gitconfig"
echo 'set -o vi' > "$HOME/.inputrc"
"$W" adopt-dotfiles --yes > /dev/null
FILES=$("$W" files)
check "adopt tracked the found dotfiles" "echo \"\$FILES\" | grep -q gitconfig && echo \"\$FILES\" | grep -q inputrc"

echo "=== diff + log"
echo 'export Z=9' >> "$HOME/.zshrc"
DIFF=$("$W" diff ~/.zshrc)
check "diff shows the unsettled change" "echo \"\$DIFF\" | grep -q '+export Z=9'"
sleep 2.5
check "diff clean after settle" "\"$W\" diff ~/.zshrc | grep -q 'matches the store'"
LOG=$("$W" log ~/.zshrc)
check "log lists real history" "[ \"\$(echo \"\$LOG\" | wc -l | tr -d ' ')\" -ge 3 ]"

echo "=== status knows the last push"
check "status reports last push age" "\"$W\" status | grep 'last push' | grep -q 'ago'"

echo "=== restore"
rm "$HOME/.zshrc"
"$W" restore ~/.zshrc > /dev/null
check "restore brings the file back" "grep -q 'export C=3' '$HOME/.zshrc'"

kill $DPID 2>/dev/null
echo
echo "RESULTS: $pass passed, $fail failed"
if [ $fail -eq 0 ]; then echo "DRILL CLEAN"; else echo "daemon log:"; tail -20 "$ROOT/daemon.log"; exit 1; fi
