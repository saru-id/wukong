#!/bin/bash
# The dress rehearsal: `wukong init` end to end, exactly as day one
# will run it — machine A from nothing (real starter config, real
# adopt), machine B joining by clone (real sync offer). Deliberately
# NON-hermetic on machine A: it reads the REAL machine's package
# receipts (read-only; every write stays in the sandbox) because this
# drill's job is the real flow. Also home of the founding-constraint
# check: the idle daemon must stay small and quiet.
# WUKONG_BIN_DIR overrides the binaries — how the release workflow
# rehearses the actual tarball.
set -u
cd "$(dirname "$0")/.." || exit 1
if [ -n "${WUKONG_BIN_DIR:-}" ]; then
  BIN="$WUKONG_BIN_DIR"
else
  cargo build -q -p wukong -p wukongd || exit 1
  BIN="${CARGO_TARGET_DIR:-target}/debug"
fi
BIN="$(cd "$BIN" && pwd)"
W="$BIN/wukong"

ROOT=$(mktemp -d)
git init -q --bare -b main "$ROOT/remote.git"
pass=0; fail=0
ok()   { pass=$((pass+1)); echo "  PASS  $1"; }
bad()  { fail=$((fail+1)); echo "  FAIL  $1"; }
check(){ if eval "$2"; then ok "$1"; else bad "$1"; fi }

echo "=== machine A: init from nothing, --yes all the way"
export HOME="$ROOT/a/home"
export XDG_CONFIG_HOME="$HOME/.config"
export XDG_DATA_HOME="$HOME/.local/share"
export XDG_STATE_HOME="$HOME/.local/state"
mkdir -p "$HOME"
INIT_A=$(printf '%s\n' "$ROOT/remote.git" | WUKONG_NO_AGENT=1 "$W" init --yes 2>&1)
check "starter config written" "echo \"$INIT_A\" | grep -q '✓ config'"
check "store repo created" "echo \"$INIT_A\" | grep -q '✓ store repo'"
check "daemon started without launchd" "echo \"$INIT_A\" | grep -q 'WUKONG_NO_AGENT'"
check "adopt offered and ran" "echo \"$INIT_A\" | grep -q 'Taking in'"
APID=$(cat "$XDG_STATE_HOME/wukong/wukongd.pid")
check "daemon answers" "\"$W\" status > /dev/null 2>&1"
check "config file is the commented manual" "grep -q '# Pin any provider' \"$XDG_CONFIG_HOME/wukong/config.toml\""

echo "=== the founding constraint: small and quiet at idle"
sleep 2
RSS_KB=$(ps -o rss= -p "$APID" | tr -d ' ')
check "idle RSS under 64MB (measured ${RSS_KB}KB)" "[ \"$RSS_KB\" -lt 65536 ]"
CPU0=$(ps -o cputime= -p "$APID" | tr -d ' ')
sleep 10
CPU1=$(ps -o cputime= -p "$APID" | tr -d ' ')
DELTA=$(python3 - "$CPU0" "$CPU1" <<'PY'
import sys
def secs(t):
    parts = t.split(":")
    out = 0.0
    for p in parts:
        out = out * 60 + float(p)
    return out
print(f"{secs(sys.argv[2]) - secs(sys.argv[1]):.2f}")
PY
)
check "idle CPU under 0.5s over 10s (used ${DELTA}s)" "python3 -c \"exit(0 if $DELTA < 0.5 else 1)\""

echo "=== machine A: govern and share"
printf '[user]\n\tname = day-one\n' > "$HOME/.dayone"
"$W" track "$HOME/.dayone" > /dev/null
sleep 2.5
"$W" share "$HOME/.dayone" > /dev/null
"$W" push > /dev/null
check "remote has the shared branch" "git -C '$ROOT/remote.git' rev-parse --verify -q shared > /dev/null"
DEEP_A=$("$W" doctor --deep)
check "deep doctor: fsck passes" "echo \"$DEEP_A\" | grep -q '✓ store passes git fsck'"
check "deep doctor: dry-run restore answers" "echo \"$DEEP_A\" | grep -q '✓ dry-run restore answers'"
kill "$APID" 2>/dev/null; sleep 0.5

echo "=== machine B: init joins by clone (hermetic side)"
export HOME="$ROOT/b/home"
export XDG_CONFIG_HOME="$HOME/.config"
export XDG_DATA_HOME="$HOME/.local/share"
export XDG_STATE_HOME="$HOME/.local/state"
mkdir -p "$XDG_CONFIG_HOME/wukong"
cat > "$XDG_CONFIG_HOME/wukong/config.toml" <<EOF
machine = "sandbox-b"
remote = "$ROOT/remote.git"
debounce_secs = 1
push_interval_secs = 3600
sentinels = []
notifications = false

[packages]
enabled = false
EOF
INIT_B=$(WUKONG_NO_AGENT=1 "$W" init < /dev/null 2>&1)
check "B cloned the store" "echo \"$INIT_B\" | grep -q 'cloned store'"
check "B was offered the sync" "echo \"$INIT_B\" | grep -q 'syncing it on'"
check "sync plan sees the shared file" "echo \"$INIT_B\" | grep -q '1 to restore'"
check "declined sync changed nothing" "[ ! -f \"$HOME/.dayone\" ]"
BPID=$(cat "$XDG_STATE_HOME/wukong/wukongd.pid")
check "B machine branch started empty" "[ \"$(git -C "$XDG_DATA_HOME/wukong/store" ls-files | wc -l | tr -d ' ')\" = 0 ]"

"$W" restore > /dev/null 2>&1
check "restore landed the shared file" "grep -q 'day-one' \"$HOME/.dayone\""
check "lane survived the trip" "\"$W\" files | grep dayone | grep -q '(shared)'"
kill "$BPID" 2>/dev/null

echo
echo "RESULTS: $pass passed, $fail failed"
if [ $fail -eq 0 ]; then echo "DRILL CLEAN"; else exit 1; fi
