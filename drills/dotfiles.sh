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

# Hermetic: this drill exercises one lane; the daemon must not detect
# the REAL machine's package world (or fork npm at startup).
[packages]
enabled = false

[seal]
identity_file = "$ROOT/age.key"
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

echo "=== assignment and entropy secret shapes must quarantine"
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
check "hex secret quarantined (charset-aware entropy)" "[ \"\$(sqlite3 '$DB' 'SELECT COUNT(*) FROM inbox WHERE resolved=0')\" = 1 ]"
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

echo "=== sealed lane: forbidden file, ciphertext-only store"
SEALTOK=ghp_sealedrilltokensealedrilltoken00
printf 'TOKEN=%s\n' "$SEALTOK" > "$HOME/.env"
check "plaintext tracking of .env refused" "! \"$W\" track ~/.env > /dev/null 2>&1"
"$W" track --sealed ~/.env > /dev/null
sleep 1
check "store holds age ciphertext" "head -c 21 '$STORE/.env' | grep -q 'age-encryption.org'"
check "plaintext never in store" "! grep -rq '$SEALTOK' '$STORE' --exclude-dir=.git"
check "identity NOT in the store" "[ ! -e '$STORE/age.key' ] && ! git -C '$STORE' ls-files | grep -q age.key"
check "recipient IS in the store" "git -C '$STORE' ls-files | grep -q 'age.recipient'"
COMMITS_BEFORE=$(git -C "$STORE" log --oneline -- .env | wc -l | tr -d ' ')
touch "$HOME/.env"
sleep 2.5
COMMITS_AFTER=$(git -C "$STORE" log --oneline -- .env | wc -l | tr -d ' ')
check "unchanged sealed file does not re-commit" "[ \"\$COMMITS_BEFORE\" = \"\$COMMITS_AFTER\" ]"
printf 'TOKEN=%s\nEXTRA=1\n' "$SEALTOK" > "$HOME/.env"
sleep 2.5
check "edited sealed file commits, no quarantine" "[ \"\$(git -C '$STORE' log --oneline -- .env | wc -l | tr -d ' ')\" -gt \"\$COMMITS_AFTER\" ] && [ \"\$(sqlite3 '$DB' 'SELECT COUNT(*) FROM inbox WHERE resolved=0')\" = 0 ]"
rm "$HOME/.env"
sleep 0.5
"$W" restore ~/.env --force > /dev/null 2>&1 || "$W" restore ~/.env > /dev/null
check "restore decrypts the sealed file" "grep -q '$SEALTOK' '$HOME/.env'"
check "doctor reports the seal identity unlocks this store" "\"$W\" doctor | grep -q 'seal identity unlocks this store'"

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
check "remote never saw the sealed token"      "! git -C '$ROOT/verify' log -p --all | grep -q '$SEALTOK'"
check "remote's .env is ciphertext" "head -c 21 '$ROOT/verify/.env' | grep -q 'age-encryption.org'"

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

echo "=== adopt (dotfiles + packages, one word)"
printf '[user]\n\tname = s\n' > "$HOME/.gitconfig"
echo 'set -o vi' > "$HOME/.inputrc"
"$W" adopt --yes > /dev/null
FILES=$("$W" files)
check "adopt tracked the found dotfiles" "echo \"\$FILES\" | grep -q gitconfig && echo \"\$FILES\" | grep -q inputrc"

echo "=== revert rewinds forward"
printf 'revert v1\n' > "$HOME/.rvt"
"$W" track "$HOME/.rvt" > /dev/null
sleep 2.5
printf 'revert v2\n' > "$HOME/.rvt"
sleep 2.5
"$W" revert "$HOME/.rvt" > /dev/null
sleep 2.5
check "live file rewound to the previous version" "grep -q 'revert v1' \"$HOME/.rvt\""
check "the rewind is a NEW commit, not a rewrite" "[ \"\$(\"$W\" log \"$HOME/.rvt\" | wc -l | tr -d ' ')\" = 3 ]"

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

echo "=== json output"
check "status --json parses, machine correct" "\"$W\" status --json | python3 -c 'import json,sys; d=json.load(sys.stdin); assert d[\"machine\"]==\"sandbox\"'"
check "files --json is a non-empty list" "\"$W\" files --json | python3 -c 'import json,sys; d=json.load(sys.stdin); assert isinstance(d,list) and len(d)>=3'"
check "inbox --json is a list" "\"$W\" inbox --json | python3 -c 'import json,sys; assert isinstance(json.load(sys.stdin),list)'"

echo "=== daemon log has timestamps + banner"
check "startup banner with version" "grep -q 'wukongd .* starting — governing sandbox' '$ROOT/daemon.log'"
check "log lines are timestamped" "grep -qE '^[0-9]{4}-[0-9]{2}-[0-9]{2}T' '$ROOT/daemon.log'"

echo "=== daemon status exit codes"
check "daemon status exits 0 while running" "\"$W\" daemon status > /dev/null"

echo "=== uninstall leaves cleanly"
kill $DPID 2>/dev/null
wait $DPID 2>/dev/null
"$W" uninstall --purge --yes > /dev/null 2>&1
check "purge removed the data dir" "[ ! -e \"$XDG_DATA_HOME/wukong\" ]"
check "purge removed the config" "[ ! -e \"$XDG_CONFIG_HOME/wukong\" ]"
check "purge removed the state dir" "[ ! -e \"$XDG_STATE_HOME/wukong\" ]"
check "daemon status exits 1 when gone" "! \"$W\" daemon status > /dev/null 2>&1"

echo
echo "RESULTS: $pass passed, $fail failed"
if [ $fail -eq 0 ]; then echo "DRILL CLEAN"; else echo "daemon log:"; tail -20 "$ROOT/daemon.log"; exit 1; fi
