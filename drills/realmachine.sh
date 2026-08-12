#!/bin/bash
# Opt-in drill: the things a sandbox cannot prove.
#
# NOT wired into `make drill` and NOT run in CI. Run it by hand on a
# Mac you can afford to poke:
#
#   bash drills/realmachine.sh
#
# Why it exists: every other drill runs under a fake HOME, and the
# macOS facilities wukong leans on are HOME-relative or machine-global.
# `security` resolves the login keychain through $HOME, so a sandboxed
# daemon never touches the real Keychain at all — which is exactly why
# the Keychain path shipped for twenty releases without once being run
# outside a developer's hands.
#
# So this drill splits in two. What CAN be automated is the contract
# wukong's code depends on: what `security` returns, and when it
# blocks. Those checks run against the real login keychain under a
# drill-only service name (`id.saru.wukong.seal.drill`), cleaned up at
# the end. What CANNOT be automated is the seal lane end to end — it
# needs a daemon running against your real HOME — so the drill prints
# that procedure and you run it yourself.
set -u
cd "$(dirname "$0")/.." || exit 1
[ "$(uname)" = "Darwin" ] || { echo "macOS only"; exit 1; }
cargo build -q -p wukong -p wukongd || exit 1
BIN="$(cd "${CARGO_TARGET_DIR:-target}/debug" && pwd)"

REAL_HOME="$HOME"
SERVICE="id.saru.wukong.seal.drill"
DOMAIN="id.saru.wukong.drill"
ROOT=$(mktemp -d)
KEYCHAIN_OK=0

cleanup() {
  [ "$KEYCHAIN_OK" = 1 ] &&
    security delete-generic-password -a wukong -s "$SERVICE" >/dev/null 2>&1
  defaults delete "$DOMAIN" >/dev/null 2>&1
  rm -rf "$ROOT"
}
trap cleanup EXIT

pass=0; fail=0; skip=0
ok()   { pass=$((pass+1)); echo "  PASS  $1"; }
bad()  { fail=$((fail+1)); echo "  FAIL  $1"; }
note() { skip=$((skip+1)); echo "  SKIP  $1"; }
check(){ if eval "$2"; then ok "$1"; else bad "$1"; fi }

echo "=== the keychain answers in exit codes, and they mean different things"
# Establish the keychain is UNLOCKED before writing anything to it. A
# write to a locked keychain blocks forever on an unlock prompt nobody
# is present to answer — measured at ten minutes and counting — while
# `show-keychain-info` answers in either state without prompting. A
# read is NOT a safe probe: a lookup that misses returns 44 whether the
# keychain is open or shut, which is the whole point of this section.
KEYCHAIN=$(security default-keychain -d user 2>/dev/null | tr -d ' "')
if [ -n "$KEYCHAIN" ] && security show-keychain-info "$KEYCHAIN" > /dev/null 2>&1; then
  KEYCHAIN_OK=1
fi

security find-generic-password -a wukong -s "$SERVICE.absent" -w >/dev/null 2>&1
check "a lookup that misses is errSecItemNotFound (44)" "[ $? = 44 ]"
check "44 is the code wukong reads as 'no identity'" \
  "grep -q 'ERR_SEC_ITEM_NOT_FOUND: i32 = 44' crates/core/src/seal.rs"

if [ "$KEYCHAIN_OK" = 1 ]; then
  security add-generic-password -a wukong -s "$SERVICE" -U -w "AGE-SECRET-KEY-DRILLONLY" 2>/dev/null
  OUT=$(security find-generic-password -a wukong -s "$SERVICE" -w 2>/dev/null)
  check "a stored item reads back verbatim" "[ \"$OUT\" = 'AGE-SECRET-KEY-DRILLONLY' ]"
  echo
  echo "  BY HAND — the answer wukong must never mistake for 'no identity':"
  echo "      security lock-keychain '$KEYCHAIN'"
  echo "      security find-generic-password -a wukong -s $SERVICE -w; echo \$?"
  echo "    Expect 152, not 44. Then, in another shell, confirm the write"
  echo "    path is the one that hangs (Ctrl-C it; it will not return):"
  echo "      security add-generic-password -a wukong -s $SERVICE -U -w X"
  echo "    Log back in to unlock — there is no scriptable unlock."
  echo
else
  note "the login keychain is LOCKED or absent ('$KEYCHAIN')"
  echo "        Every check that writes a key is skipped, because the write"
  echo "        would hang rather than fail. Log in again and re-run."
fi

echo "=== the seal lane, cold, against the real Keychain"
note "needs a daemon on your REAL \$HOME — 'security' finds the login"
echo "        keychain through \$HOME, so no sandboxed daemon can reach it."
echo "        Run this once on a machine with no seal identity yet:"
echo "          security find-generic-password -s id.saru.wukong.seal   # expect 44"
echo "          printf 'export T=ghp_x\\n' > ~/.sealdrill.env"
echo "          wukong track ~/.sealdrill.env --sealed && sleep 4"
echo "          security find-generic-password -s id.saru.wukong.seal   # now exists"
echo "          head -c 21 ~/.local/share/wukong/store/.sealdrill.env   # age-encryption.org"
echo "          wukong daemon restart && sleep 3 && wukong doctor --deep"
echo "          wukong untrack ~/.sealdrill.env && rm ~/.sealdrill.env"
echo "        Every step must pass with NO keychain prompt: the item is"
echo "        created and read by /usr/bin/security, so the ACL trusts it."

echo "=== real preference domains: read the plist, write through defaults"
defaults write "$DOMAIN" DrillFlag -bool true
check "a write is visible to a read immediately" \
  "[ \"\$(defaults read '$DOMAIN' DrillFlag)\" = 1 ]"
defaults write "$DOMAIN" DrillFlag -bool false
check "and so is the second one (no stale cfprefsd cache)" \
  "[ \"\$(defaults read '$DOMAIN' DrillFlag)\" = 0 ]"
defaults write "$DOMAIN" DrillFloat -float 0.35
check "a float round-trips through defaults" \
  "[ \"\$(defaults read '$DOMAIN' DrillFloat)\" = '0.35' ]"
check "the plist wukong would READ has the value defaults wrote" \
  "plutil -extract DrillFloat raw '$REAL_HOME/Library/Preferences/$DOMAIN.plist' | grep -q '0.35'"

echo "=== ByHost is a real place, and wukong does not look there"
BYHOST="$REAL_HOME/Library/Preferences/ByHost"
if [ -d "$BYHOST" ]; then
  N=$(find "$BYHOST" -name '*.plist' | wc -l | tr -d ' ')
  note "$N ByHost domain(s) on this machine — none are governable today"
  echo "        Measured, not assumed: com.apple.controlcenter's INTERESTING"
  echo "        keys are in the plain domain; ByHost holds its bentoboxes"
  echo "        layout and com.apple.loginwindow's MiniBuddyLaunch. If the"
  echo "        corpus ever grows a ByHost key, the reader must resolve the"
  echo "        hardware UUID; a plain domain read will silently miss it,"
  echo "        which reads as 'unset' rather than as an error."
else
  note "no ByHost directory on this machine"
fi

echo "=== the real binaries, with a reader that walks away"
"$BIN/wukong" doctor 2> "$ROOT/pipe.err" | true
check "no panic when a pipe closes early" "[ ! -s \"$ROOT/pipe.err\" ]"
"$BIN/wukong" settings list 2> "$ROOT/pipe2.err" | true
check "…on a second verb too" "[ ! -s \"$ROOT/pipe2.err\" ]"

echo
echo "RESULTS: $pass passed, $fail failed, $skip need a human"
[ $fail -eq 0 ] && echo "DRILL CLEAN" || exit 1
