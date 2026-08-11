#!/bin/bash
# Live drill: the shared lane across two machines and one remote.
# Machine A shares a dotfile and a package; machine B clones the store
# and receives both through `wukong sync`. The verification method of
# record for the overlay semantics.
set -u
cd "$(dirname "$0")/.." || exit 1
cargo build -q -p wukong -p wukongd || exit 1
BIN="${CARGO_TARGET_DIR:-target}/debug"
BIN="$(cd "$BIN" && pwd)"
W="$BIN/wukong"

ROOT=$(mktemp -d)
git init -q --bare -b main "$ROOT/remote.git"
pass=0; fail=0
ok()   { pass=$((pass+1)); echo "  PASS  $1"; }
bad()  { fail=$((fail+1)); echo "  FAIL  $1"; }
check(){ if eval "$2"; then ok "$1"; else bad "$1"; fi }

wait_daemon() { # up to 15s for the socket to answer
  for _ in $(seq 1 50); do
    "$W" status > /dev/null 2>&1 && return 0
    sleep 0.3
  done
  return 1
}

env_for() { # machine name
  export HOME="$ROOT/$1/home"
  export XDG_CONFIG_HOME="$HOME/.config"
  export XDG_DATA_HOME="$HOME/.local/share"
  export XDG_STATE_HOME="$HOME/.local/state"
  mkdir -p "$XDG_CONFIG_HOME/wukong" "$ROOT/$1/brew/Cellar" "$ROOT/$1/Applications"
  cat > "$XDG_CONFIG_HOME/wukong/config.toml" <<EOF
machine = "sandbox-$1"
remote = "$ROOT/remote.git"
debounce_secs = 1
push_interval_secs = 3600
sentinels = []
notifications = false

[packages]
enabled = true
brew_prefix = "$ROOT/$1/brew"
applications_dir = "$ROOT/$1/Applications"

[packages.roots]
npm = "$ROOT/absent-npm"
pnpm = "$ROOT/absent-pnpm"
bun = "$ROOT/absent-bun"
cargo = "$ROOT/absent-cargo"
go = "$ROOT/absent-go"
gem = "$ROOT/absent-gem"
pipx = "$ROOT/absent-pipx"
uv = "$ROOT/absent-uv"
dotnet = "$ROOT/absent-dotnet"
pub = "$ROOT/absent-pub"
EOF
}

echo "=== machine A: share a file and a package"
env_for a
mkdir -p "$ROOT/a/brew/Cellar/jq/1.7"
echo '{"installed_on_request":true}' > "$ROOT/a/brew/Cellar/jq/1.7/INSTALL_RECEIPT.json"
"$BIN/wukongd" > "$ROOT/a-daemon.log" 2>&1 &
APID=$!
wait_daemon || echo "  WARN  daemon A slow to answer"
STORE_A="$XDG_DATA_HOME/wukong/store"
SHARED_A="$XDG_DATA_HOME/wukong/shared"
check "shared worktree exists beside the store" "[ -e '$SHARED_A/.git' ]"

printf '[user]\n\tname = a\n' > "$HOME/.gitconfig"
"$W" track ~/.gitconfig > /dev/null
sleep 2.5
"$W" share ~/.gitconfig > /dev/null
check "mirror moved to the shared worktree" "[ -f '$SHARED_A/.gitconfig' ] && [ ! -f '$STORE_A/.gitconfig' ]"
check "files marks the lane" "\"$W\" files | grep gitconfig | grep -q '(shared)'"

printf '[user]\n\tname = a2\n' > "$HOME/.gitconfig"
sleep 2.5
check "edits now commit to the shared branch" "grep -q 'a2' '$SHARED_A/.gitconfig'"

"$W" pkg adopt-installed > /dev/null
"$W" pkg share jq > /dev/null
check "package moved to the shared manifest" "grep -q jq '$SHARED_A/__wukong__/packages.toml' && ! grep -q '\"jq\"' '$STORE_A/__wukong__/packages.toml'"
check "pkg list marks the lane" "\"$W\" pkg list | grep jq | grep -q '(shared)'"

"$W" push > /dev/null
check "remote received the shared branch" "git -C '$ROOT/remote.git' rev-parse --verify -q shared > /dev/null"
kill $APID 2>/dev/null; sleep 0.5

echo "=== machine B: clone, sync, receive"
env_for b
STORE_B="$XDG_DATA_HOME/wukong/store"
SHARED_B="$XDG_DATA_HOME/wukong/shared"
# What `wukong init` does on a new machine, minus launchd: clone, then
# start the machine branch EMPTY (files come via adopt or shared).
git clone -q "$ROOT/remote.git" "$STORE_B" 2> /dev/null
git -C "$STORE_B" config user.name wukong
git -C "$STORE_B" config user.email wukong@sandbox-b
EMPTY=$(git -C "$STORE_B" mktree < /dev/null)
CROOT=$(git -C "$STORE_B" commit-tree "$EMPTY" -m "machine root")
git -C "$STORE_B" checkout -q -B sandbox-b "$CROOT"
"$BIN/wukongd" > "$ROOT/b-daemon.log" 2>&1 &
BPID=$!
wait_daemon || echo "  WARN  daemon B slow to answer"
check "B's shared worktree tracks origin" "grep -q 'a2' '$SHARED_B/.gitconfig'"

PLAN=$("$W" sync --dry-run)
check "sync plan restores the shared file" "echo \"$PLAN\" | grep -q '1 to restore'"
check "sync plan installs the shared package" "echo \"$PLAN\" | grep -q 'brew install jq'"

"$W" restore > /dev/null 2>&1
check "shared file restored onto B" "grep -q 'a2' \"$HOME/.gitconfig\""
check "restored file re-marked shared" "\"$W\" files | grep gitconfig | grep -q '(shared)'"

printf '[user]\n\tname = b\n' > "$HOME/.gitconfig"
sleep 2.5
check "B's edit lands on the shared branch" "grep -q 'name = b' '$SHARED_B/.gitconfig'"
"$W" push > /dev/null
check "remote shared carries B's edit" "git -C '$ROOT/remote.git' show shared:.gitconfig | grep -q 'name = b'"

kill $BPID 2>/dev/null
echo
echo "RESULTS: $pass passed, $fail failed"
if [ $fail -eq 0 ]; then echo "DRILL CLEAN"; else echo "A log:"; tail -10 "$ROOT/a-daemon.log"; echo "B log:"; tail -10 "$ROOT/b-daemon.log"; exit 1; fi
