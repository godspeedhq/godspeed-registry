#!/usr/bin/env bash
# The developer's side, end to end: the real `godspeed` CLI logging in to, and publishing to, this
# registry running locally under `wrangler dev`. Never touches Cloudflare.
#
#   GODSPEED_OS=<GodspeedOS checkout> scripts/cli-test.sh <path-to-godspeed-binary>
#
# It creates a fresh service project, publishes it, publishes it again, and checks every outcome a developer
# can meet: published, already published (identical), version exists (different), namespace not theirs,
# logged out.
set -u
cd "$(dirname "$0")/.."
REGDIR=$(pwd)
admin() { (cd "$REGDIR" && node scripts/admin.mjs "$@" > /dev/null); }
GODSPEED=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
: "${GODSPEED_OS:?set GODSPEED_OS to a GodspeedOS checkout}"
PORT=${PORT:-8788}
URL="http://127.0.0.1:$PORT"
WRANGLER="node node_modules/wrangler/bin/wrangler.js"
WORK=$(mktemp -d)
export GODSPEED_CONFIG_DIR="$WORK/config"
unset GODSPEED_REGISTRY GODSPEED_TOKEN

pass=0
fail=0
check() { # check <wanted exit> <text the output must contain> <description> -- <command...>
  local want=$1 text=$2 what=$3
  shift 4
  local out
  out=$("$@" 2>&1)
  local got=$?
  if [ "$got" = "$want" ] && printf '%s' "$out" | tr -s ' \n' '  ' | grep -qF "$text"; then
    pass=$((pass + 1)); echo "ok    $what"
  else
    fail=$((fail + 1)); echo "FAIL  $what (exit $got, wanted $want, looking for \"$text\"):"; printf '%s\n' "$out" | tail -15
  fi
}

rm -rf .wrangler/state
$WRANGLER d1 migrations apply godspeed-registry --local > /dev/null
node scripts/admin.mjs add-publisher alice alice > /dev/null
node scripts/admin.mjs add-publisher bob bob > /dev/null
TOKEN=$(node scripts/admin.mjs issue-token alice --days 1 | tail -1)

$WRANGLER dev --port "$PORT" --ip 127.0.0.1 > "$WORK/dev.log" 2>&1 &
DEV=$!
trap 'kill $DEV 2>/dev/null; rm -rf "$WORK"' EXIT
curl -s --retry 150 --retry-connrefused --retry-delay 2 --retry-all-errors -o /dev/null "$URL/" \
  || { echo "the Worker did not start:"; cat "$WORK/dev.log"; exit 1; }

cd "$WORK"
"$GODSPEED" new service greeter > /dev/null
cd greeter
sed -i 's/^# publisher = "your-name"/publisher = "alice"/' godspeed.toml

check 18 "not logged in" "publish before logging in is refused, before building" -- "$GODSPEED" publish --registry "$URL"
check 18 "not one the registry issued" "a token the registry never issued is refused at login" -- \
  sh -c "echo gsp_0000000000000000 | '$GODSPEED' login --registry '$URL'"
check 0 "Logged in to $URL as alice" "login with the issued token" -- \
  sh -c "echo '$TOKEN' | '$GODSPEED' login --registry '$URL'"
check 0 "may publish under: alice/" "whoami names the account and its namespaces" -- "$GODSPEED" whoami

# Signing: no key, then a key the registry does not know, then a registered one.
check 11 "You have no signing key" "publishing without a signing key is refused, before building" -- "$GODSPEED" publish
check 0 "Generated a signing key" "key generate makes a key and prints only the public half" -- "$GODSPEED" key generate
check 24 "already exists" "key generate never replaces an existing key" -- "$GODSPEED" key generate
KEY=$("$GODSPEED" key show 2> /dev/null)
check 23 "is not registered for signing under alice/" "a release signed by an unregistered key is refused" -- "$GODSPEED" publish
admin add-key alice "$KEY"
check 0 "Published alice/greeter@0.1.0" "once the key is registered, publish resumes the same upload and finalises" -- "$GODSPEED" publish
check 0 "already published, with this exact manifest" "publishing the same thing again is a success, not a conflict" -- "$GODSPEED" publish

mkdir -p "$WORK/download"
for f in manifest.toml manifest.sig contract.toml x86_64.elf aarch64.elf armv7.elf riscv64.elf; do
  curl -s -o "$WORK/download/$f" "$URL/packages/alice/greeter/0.1.0/$f"
done
check 0 "signed by $KEY" "the published package, downloaded, verifies, signed by the registered key" -- "$GODSPEED" verify "$WORK/download"

# A revoked key signs nothing new; what it already signed stays published.
admin revoke-key alice "$KEY"
sed -i 's/^version = "0.1.0"/version = "0.1.1"/' Cargo.toml "contracts/greeter.toml"
check 23 "is not registered for signing under alice/" "after its key is revoked, a new release is refused" -- "$GODSPEED" publish
check 0 "Verified alice/greeter 0.1.0" "what the revoked key signed before stays downloadable and verifiable" -- "$GODSPEED" verify "$WORK/download"
sed -i 's/^version = "0.1.1"/version = "0.1.0"/' Cargo.toml "contracts/greeter.toml"

sed -i 's/^description = .*/description = "changed after publishing"/' godspeed.toml
check 24 "Published versions never change" "a different package under a published version is refused" -- "$GODSPEED" publish

sed -i 's/^publisher = "alice"/publisher = "bob"/' godspeed.toml
check 23 "may not publish under bob/" "publishing under someone else's namespace is refused" -- "$GODSPEED" publish

check 0 "the token is revoked" "logout revokes the token" -- "$GODSPEED" logout
check 18 "not logged in" "after logout, publishing needs a login again" -- "$GODSPEED" publish --registry "$URL"

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
