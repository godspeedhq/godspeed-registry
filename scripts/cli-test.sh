#!/usr/bin/env bash
# The developer's side, end to end: the real `godspeed` CLI logging in to, and publishing to, this
# registry running locally under `wrangler dev`. Never touches Cloudflare.
#
#   GODSPEED_OS=<GodspeedOS checkout> scripts/cli-test.sh <path-to-godspeed-binary>
#
# It creates a fresh service project and takes it through every outcome a developer can meet: no key, an
# unlisted key, published, already published (identical), verified by `fetch` through the registry's trust
# anchor, a key RETIRED (its earlier release stays trusted; it signs nothing new; a new key takes over), a key
# REVOKED (nothing it signed is trusted), version exists, namespace not theirs, logged out. Keys are listed,
# retired and revoked with the offline tool, exactly as the maintainer would.
set -u
cd "$(dirname "$0")/.."
REGDIR=$(pwd)
GODSPEED=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
: "${GODSPEED_OS:?set GODSPEED_OS to a GodspeedOS checkout}"
PORT=${PORT:-8788}
URL="http://127.0.0.1:$PORT"
WRANGLER="node node_modules/wrangler/bin/wrangler.js"
WORK=$(mktemp -d)
export GODSPEED_CONFIG_DIR="$WORK/config"
unset GODSPEED_REGISTRY GODSPEED_TOKEN GODSPEED_SIGNING_KEY

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
# Change the signed namespaces with the offline tool, from the registry directory.
change() { (cd "$REGDIR" && trust_change "$URL" "$@"); }

rm -rf .wrangler/state
$WRANGLER d1 migrations apply godspeed-registry --local > /dev/null
node scripts/admin.mjs add-publisher alice alice > /dev/null
node scripts/admin.mjs add-publisher bob bob > /dev/null
# A trust anchor that, to start with, lists only a key of bob's: alice has none yet.
GODSPEED_CONFIG_DIR="$WORK/bob-config" "$GODSPEED" key generate > /dev/null
# shellcheck source=trust-setup.sh
source scripts/trust-setup.sh "$WORK" bob "$(GODSPEED_CONFIG_DIR="$WORK/bob-config" "$GODSPEED" key show 2> /dev/null)"
set +e

$WRANGLER dev --port "$PORT" --ip 127.0.0.1 > "$WORK/dev.log" 2>&1 &
DEV=$!
trap 'kill $DEV 2>/dev/null; rm -rf "$WORK"' EXIT
curl -s --retry 150 --retry-connrefused --retry-delay 2 --retry-all-errors -o /dev/null "$URL/" \
  || { echo "the Worker did not start:"; cat "$WORK/dev.log"; exit 1; }
curl -s -X POST -o /dev/null "$URL/v1/trust/refresh"

cd "$WORK"
"$GODSPEED" new service greeter > /dev/null
cd greeter
sed -i 's/^# publisher = "your-name"/publisher = "alice"/' godspeed.toml

check 18 "not logged in" "publish before logging in is refused, before building" -- "$GODSPEED" publish --registry "$URL"
check 18 "has not approved this token" "login --with-token refuses a token nobody approved, and stores nothing" -- \
  sh -c "echo gsp_0000000000000000 | '$GODSPEED' login --with-token --registry '$URL'"

# The publisher makes their token; the maintainer approves only its fingerprint.
check 0 "Send the registry maintainer this fingerprint" "login makes a token here and prints its fingerprint" -- \
  "$GODSPEED" login --registry "$URL"
FINGERPRINT=$("$GODSPEED" login 2> /dev/null | grep -o 'sha256:[0-9a-f]\{64\}' | head -1)
check 18 "has not approved this token" "before approval, whoami says the token is not approved" -- "$GODSPEED" whoami
check 0 "waiting for the registry maintainer" "login again before approval keeps the same token" -- "$GODSPEED" login
(cd "$REGDIR" && node scripts/admin.mjs approve-token alice "$FINGERPRINT" --days 1 > /dev/null)
check 0 "Already logged in to $URL as alice" "once the fingerprint is approved, login confirms the account" -- "$GODSPEED" login
check 0 "may publish under: alice/" "whoami names the account and its namespaces" -- "$GODSPEED" whoami

# Signing: no key, then a key the signed namespaces do not list, then a listed one.
check 11 "You have no signing key" "publishing without a signing key is refused, before building" -- "$GODSPEED" publish
check 0 "Generated a signing key" "key generate makes a key and prints only the public half" -- "$GODSPEED" key generate
check 24 "already exists" "key generate never replaces an existing key" -- "$GODSPEED" key generate
KEY=$("$GODSPEED" key show 2> /dev/null)
check 23 "not one of alice/'s keys" "a release signed by a key the signed namespaces do not list is refused" -- "$GODSPEED" publish
change add alice "$KEY"
check 0 "Published alice/greeter@0.1.0" "once the key is listed, publish resumes the same upload and finalises" -- "$GODSPEED" publish
check 0 "already published, with this exact manifest" "publishing the same thing again is a success, not a conflict" -- "$GODSPEED" publish
check 0 "signed by $KEY (an active key of alice/)" "fetch verifies the release through the whole chain of trust" -- \
  "$GODSPEED" fetch alice/greeter@0.1.0 --registry "$URL" --out "$WORK/fetch-1"

mkdir -p "$WORK/download"
for f in manifest.toml manifest.sig contract.toml x86_64.elf aarch64.elf armv7.elf riscv64.elf; do
  curl -s -o "$WORK/download/$f" "$URL/packages/alice/greeter/0.1.0/$f"
done
check 0 "signed by $KEY" "the published package, downloaded, verifies offline too" -- "$GODSPEED" verify "$WORK/download"

# RETIRE: a routine rotation. The old key signs nothing new; what it published before stays trusted.
sleep 1 # a retirement is recorded to the second: make it strictly later than 0.1.0's publication
GODSPEED_CONFIG_DIR="$WORK/config-b" "$GODSPEED" key generate > /dev/null
KEY_B=$(GODSPEED_CONFIG_DIR="$WORK/config-b" "$GODSPEED" key show 2> /dev/null)
SECRET_B=$(sed -n 's/^secret = "\(.*\)"$/\1/p' "$WORK/config-b/signing-key.toml")
change retire alice "$KEY"
change add alice "$KEY_B"
check 0 "retired since" "after the key is retired, what it published before still verifies" -- \
  "$GODSPEED" fetch alice/greeter@0.1.0 --registry "$URL" --out "$WORK/fetch-2"
sed -i 's/^version = "0.1.0"/version = "0.1.1"/' Cargo.toml "contracts/greeter.toml"
check 23 "was retired for alice/" "a retired key cannot sign a new release" -- "$GODSPEED" publish
check 0 "Published alice/greeter@0.1.1" "the new key publishes the next release" -- \
  env GODSPEED_SIGNING_KEY="$SECRET_B" "$GODSPEED" publish
check 0 "signed by $KEY_B (an active key of alice/)" "the new release verifies, signed by the new key" -- \
  "$GODSPEED" fetch alice/greeter@0.1.1 --registry "$URL" --out "$WORK/fetch-3"

# REVOKE: a compromise. Nothing the old key ever signed is trusted, however long ago; the new key's is.
change revoke alice "$KEY"
check 27 "signed by a revoked key" "after the key is revoked, what it signed is refused" -- \
  "$GODSPEED" fetch alice/greeter@0.1.0 --registry "$URL" --out "$WORK/fetch-4"
check 0 "signed by $KEY_B" "a release by the other key is unaffected" -- \
  "$GODSPEED" fetch alice/greeter@0.1.1 --registry "$URL" --out "$WORK/fetch-5"
check 0 "Fetched alice/greeter@0.1.1" "with no version, fetch takes the highest published" -- \
  "$GODSPEED" fetch alice/greeter --registry "$URL" --out "$WORK/fetch-6"
sed -i 's/^version = "0.1.1"/version = "0.1.0"/' Cargo.toml "contracts/greeter.toml"

sed -i 's/^description = .*/description = "changed after publishing"/' godspeed.toml
check 24 "Published versions never change" "a different package under a published version is refused" -- "$GODSPEED" publish

sed -i 's/^publisher = "alice"/publisher = "bob"/' godspeed.toml
check 23 "may not publish under bob/" "publishing under someone else's namespace is refused" -- "$GODSPEED" publish

check 0 "the token is revoked" "logout revokes the token" -- "$GODSPEED" logout
check 18 "not logged in" "after logout, publishing needs a login again" -- "$GODSPEED" publish --registry "$URL"

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
