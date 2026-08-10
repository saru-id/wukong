#!/bin/bash
# Live drill: package governance against the real daemon, with a fake
# brew tree and Applications dir inside the sandbox. The verification
# method of record for the reconcile state machine.
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
mkdir -p "$XDG_CONFIG_HOME/wukong" "$ROOT/brew/Cellar" "$ROOT/brew/Caskroom" "$ROOT/Applications"
git init -q --bare -b main "$ROOT/remote.git"

receipt() { # name on_request
  mkdir -p "$ROOT/brew/Cellar/$1/1.0"
  echo "{\"installed_on_request\":$2}" > "$ROOT/brew/Cellar/$1/1.0/INSTALL_RECEIPT.json"
}

# jq exists BEFORE the daemon starts — must baseline silently.
receipt jq true

cat > "$XDG_CONFIG_HOME/wukong/config.toml" <<EOF
machine = "sandbox"
remote = "$ROOT/remote.git"
debounce_secs = 1
push_interval_secs = 3600
sentinels = ["~/.zshrc"]
notifications = false

[packages]
enabled = true
brew_prefix = "$ROOT/brew"
applications_dir = "$ROOT/Applications"

[packages.roots]
npm = "$ROOT/npmroot"
cargo = "$ROOT/cargohome"
go = "$ROOT/gobin"
gem = "$ROOT/gemhome"
dotnet = "$ROOT/dotnetstore"
pub = "$ROOT/pubcache"
pnpm = "$ROOT/absent-pnpm"
bun = "$ROOT/absent-bun"
pipx = "$ROOT/absent-pipx"
uv = "$ROOT/absent-uv"
EOF
mkdir -p "$ROOT/npmroot/typescript" "$ROOT/npmroot/.bin" "$ROOT/cargohome" \
         "$ROOT/gobin" "$ROOT/gemhome/specifications" "$ROOT/dotnetstore/cake/4.0.0" \
         "$ROOT/pubcache/melos" "$ROOT/Applications/Bought.app/Contents/_MASReceipt"
printf '[v1]\n"ripgrep 14.1.0 (registry+https://github.com/rust-lang/crates.io-index)" = ["rg"]\n' > "$ROOT/cargohome/.crates.toml"
printf 'Gem::Specification' > "$ROOT/gemhome/specifications/colorls-1.4.6.gemspec"
printf 'apple' > "$ROOT/Applications/Bought.app/Contents/_MASReceipt/receipt"
# A synthetic Go binary: the real buildinfo blob layout, tiny.
python3 - "$ROOT/gobin/fzf" <<'PY'
import sys
def vs(b): return bytes([len(b)]) + b
sent = b"\xab" * 16
mod = sent + b"path\tgithub.com/junegunn/fzf\nmod\tgithub.com/junegunn/fzf\tv0.46.1\th1:x=\n" + sent
blob = b"junk-before" + b"\xff Go buildinf:" + bytes([8, 2]) + vs(b"go1.24.0") + vs(mod)
open(sys.argv[1], "wb").write(blob)
PY

DB="$XDG_DATA_HOME/wukong/wukong.db"
STORE="$XDG_DATA_HOME/wukong/store"
MANIFEST="$STORE/__wukong__/packages.toml"
W="$BIN/wukong"
pass=0; fail=0
ok()   { pass=$((pass+1)); echo "  PASS  $1"; }
bad()  { fail=$((fail+1)); echo "  FAIL  $1"; }
check(){ if eval "$2"; then ok "$1"; else bad "$1"; fi }
inbox_count(){ sqlite3 "$DB" "SELECT COUNT(*) FROM inbox WHERE resolved=0"; }

"$BIN/wukongd" > "$ROOT/daemon.log" 2>&1 &
DPID=$!
sleep 1.5

echo "=== baseline"
check "pre-existing jq baselined silently" "[ \"$(inbox_count)\" = 0 ]"

echo "=== new install detected via watcher"
receipt ripgrep true
receipt oniguruma false
sleep 3
check "ripgrep offered for adoption" "sqlite3 '$DB' \"SELECT subject FROM inbox WHERE resolved=0\" | grep -q 'formula:ripgrep'"
check "dependency oniguruma NOT offered" "! sqlite3 '$DB' 'SELECT subject FROM inbox' | grep -q oniguruma"
check "exactly one offer" "[ \"\$(inbox_count)\" = 1 ]"

echo "=== adopt via resolve"
ID=$(sqlite3 "$DB" "SELECT id FROM inbox WHERE resolved=0 LIMIT 1")
"$W" resolve "$ID" approve > /dev/null
check "manifest contains ripgrep" "grep -q 'ripgrep' '$MANIFEST'"
check "manifest committed under packages banner" "git -C '$STORE' log --oneline | grep -q 'packages: +ripgrep'"

echo "=== app appears, permanent ignore"
mkdir -p "$ROOT/Applications/SomeTool.app"
sleep 3
check "app offered" "sqlite3 '$DB' \"SELECT subject FROM inbox WHERE resolved=0\" | grep -q 'app:SomeTool'"
ID=$(sqlite3 "$DB" "SELECT id FROM inbox WHERE resolved=0 LIMIT 1")
"$W" resolve "$ID" never > /dev/null
check "ignore recorded in manifest" "grep -q 'SomeTool' '$MANIFEST'"
rm -rf "$ROOT/Applications/SomeTool.app"; sleep 3
mkdir -p "$ROOT/Applications/SomeTool.app"; sleep 3
check "reinstalled ignored app NOT re-offered" "[ \"\$(inbox_count)\" = 0 ]"

echo "=== manifest member vanishes"
rm -rf "$ROOT/brew/Cellar/ripgrep"
sleep 3
check "removal offered" "sqlite3 '$DB' \"SELECT kind FROM inbox WHERE resolved=0\" | grep -q 'package-gone'"
ID=$(sqlite3 "$DB" "SELECT id FROM inbox WHERE resolved=0 LIMIT 1")
"$W" resolve "$ID" approve > /dev/null
check "manifest dropped ripgrep" "! grep -q ripgrep '$MANIFEST'"

echo "=== language providers: watcher-driven adoption"
mkdir -p "$ROOT/npmroot/@biomejs/biome"
sleep 3
check "scoped npm package offered" "sqlite3 '$DB' \"SELECT subject FROM inbox WHERE resolved=0\" | grep -q 'npm:@biomejs/biome'"
ID=$(sqlite3 "$DB" "SELECT id FROM inbox WHERE resolved=0 AND subject LIKE 'npm:%' LIMIT 1")
"$W" resolve "$ID" approve > /dev/null
check "npm section lands in the manifest" "grep -q '@biomejs/biome' '$MANIFEST'"

echo "=== pkg sync --dry-run speaks each provider's language"
"$W" pkg adopt-installed > /dev/null
rm -rf "$ROOT/npmroot/@biomejs/biome" "$ROOT/brew/Cellar/jq"
sleep 3
for GID in $(sqlite3 "$DB" "SELECT id FROM inbox WHERE resolved=0"); do "$W" resolve "$GID" skip > /dev/null; done
PLAN=$("$W" pkg sync --dry-run)
check "dry-run plans npm install -g" "echo \"\$PLAN\" | grep -q 'npm install -g @biomejs/biome'"
check "dry-run plans brew install" "echo \"\$PLAN\" | grep -q 'brew install jq'"
check "dry-run executes nothing" "[ ! -d \"$ROOT/npmroot/@biomejs/biome\" ]"

echo "=== pkg list + bulk adopt (via the one-word adopt)"
"$W" adopt --yes > /dev/null
check "bulk adopt took baseline jq" "grep -q '\"jq\"' '$MANIFEST'"
LIST=$("$W" pkg list)
check "pkg list marks removed jq as missing" "echo \"$LIST\" | grep -q '^! jq'"
check "pkg list shows the npm provider" "echo \"$LIST\" | grep -q npm"

echo "=== expanded providers: receipts all the way down"
check "bulk adopt took the go module path" "grep -q 'github.com/junegunn/fzf' '$MANIFEST'"
check "bulk adopt took gem, dotnet, pub" "grep -q 'colorls' '$MANIFEST' && grep -q 'cake' '$MANIFEST' && grep -q 'melos' '$MANIFEST'"
mkdir -p "$ROOT/Applications/NewBuy.app/Contents/_MASReceipt"
printf 'apple' > "$ROOT/Applications/NewBuy.app/Contents/_MASReceipt/receipt"
sleep 3
check "App Store app offered as mas, not app" "sqlite3 '$DB' \"SELECT subject FROM inbox WHERE resolved=0\" | grep -q 'mas:NewBuy'"
MID=$(sqlite3 "$DB" "SELECT id FROM inbox WHERE resolved=0 AND subject LIKE 'mas:%' LIMIT 1")
"$W" resolve "$MID" approve > /dev/null
check "mas adoption lands in the manifest" "grep -q 'NewBuy' '$MANIFEST'"

rm "$ROOT/gobin/fzf" "$ROOT/gemhome/specifications/colorls-1.4.6.gemspec"
rm -rf "$ROOT/Applications/NewBuy.app"
sleep 3
for GID in $(sqlite3 "$DB" "SELECT id FROM inbox WHERE resolved=0"); do "$W" resolve "$GID" skip > /dev/null; done
PLAN=$("$W" pkg sync --dry-run)
check "dry-run plans go install @latest" "echo \"\$PLAN\" | grep -q 'go install github.com/junegunn/fzf@latest'"
check "dry-run plans gem install --user-install" "echo \"\$PLAN\" | grep -q 'gem install --user-install colorls'"
check "id-less App Store app comes back as a checklist" "echo \"\$PLAN\" | grep -q 'App Store apps with no recorded id'"

echo "=== providers table"
PROV=$("$W" pkg providers)
check "go root is watching" "echo \"\$PROV\" | grep 'go' | grep -q watching"
check "absent pnpm override reads off" "echo \"\$PROV\" | grep 'pnpm' | grep -q off"
check "mas rides the Applications root" "echo \"\$PROV\" | grep 'mas' | grep -q Applications"
check "table sums the active providers" "echo \"\$PROV\" | grep -q 'of 14 providers active'"

echo "=== dotfiles still governed (regression)"
echo 'export A=1' > "$HOME/.zshrc"
"$W" track ~/.zshrc > /dev/null
sleep 2.5
check "dotfile flow intact" "grep -q 'export A=1' '$STORE/.zshrc'"

echo "=== wukong sync: one plan for everything"
SYNCPLAN=$("$W" sync --dry-run)
check "sync plan has a files section" "echo \"\$SYNCPLAN\" | grep -q '^files'"
check "sync plan counts the tracked file in sync" "echo \"\$SYNCPLAN\" | grep -q 'already match'"
check "sync plan quotes package commands" "echo \"\$SYNCPLAN\" | grep -q 'npm install -g @biomejs/biome'"
check "sync dry-run executes nothing" "echo \"\$SYNCPLAN\" | grep -q 'dry run'"

echo "=== provider:name shorthand"
RMOUT=$("$W" rm go:github.com/junegunn/fzf 2>&1 || true)
check "inline provider spelling reaches the right provider" "echo \"\$RMOUT\" | grep -q 'go has no uninstall command'"

echo "=== push carries the manifest"
"$W" push > /dev/null
git clone -q -b sandbox "$ROOT/remote.git" "$ROOT/verify"
check "remote has the manifest" "grep -q '\"jq\"' '$ROOT/verify/__wukong__/packages.toml'"

echo "=== restore never touches __wukong__"
"$W" restore > /dev/null 2>&1
check "restore skips wukong namespace" "[ ! -e \"$HOME/__wukong__\" ]"

kill $DPID 2>/dev/null
echo
echo "RESULTS: $pass passed, $fail failed"
if [ $fail -eq 0 ]; then echo "DRILL CLEAN"; else echo "daemon log:"; tail -20 "$ROOT/daemon.log"; exit 1; fi
