#!/usr/bin/env bash
# Shared by local-test.sh and cli-test.sh: give a FRESH local registry a trust anchor, the way the maintainer
# does for the real one - offline keys made with godspeed-trust, root 1 and namespaces signed with them,
# uploaded with admin.mjs upload-trust, and the online key in .dev.vars for `wrangler dev`.
#
#   source scripts/trust-setup.sh <work-dir> <publisher> <public-key>
#
# Leaves TRUST_DIR, TRUST_KEYS and GODSPEED_TRUST (the tool) set. Local only: never run with --remote.
set -eu
_work=$1
_publisher=$2
_key=$3
cargo build -q -p godspeed-trust
GODSPEED_TRUST="$(pwd)/target/debug/godspeed-trust"
[ -x "$GODSPEED_TRUST" ] || GODSPEED_TRUST="$GODSPEED_TRUST.exe"
TRUST_DIR="$_work/trust"
TRUST_KEYS="$_work/trust-keys"
mkdir -p "$TRUST_KEYS"
for k in root namespaces online; do "$GODSPEED_TRUST" keygen "$TRUST_KEYS/$k.key" > "$TRUST_KEYS/$k.pub" 2> /dev/null; done
_pub() { cat "$TRUST_KEYS/$1.pub"; }
"$GODSPEED_TRUST" root --dir "$TRUST_DIR" --expires 2099-01-01T00:00:00Z \
  --role "root=1:$(_pub root)" --role "namespaces=1:$(_pub namespaces)" \
  --role "snapshot=1:$(_pub online)" --role "timestamp=1:$(_pub online)" > /dev/null
"$GODSPEED_TRUST" sign "$TRUST_DIR/root/1.toml" --key "$TRUST_KEYS/root.key" > /dev/null
"$GODSPEED_TRUST" namespaces --dir "$TRUST_DIR" add "$_publisher" "$_key" > /dev/null
"$GODSPEED_TRUST" sign "$TRUST_DIR/namespaces.toml" --key "$TRUST_KEYS/namespaces.key" > /dev/null
"$GODSPEED_TRUST" verify --dir "$TRUST_DIR" > /dev/null
node scripts/admin.mjs upload-trust "$TRUST_DIR" > /dev/null
echo "ONLINE_SIGNING_KEY=$(sed -n 's/^secret = "\(.*\)"$/\1/p' "$TRUST_KEYS/online.key")" > .dev.vars

# Change namespaces with the offline tool, re-sign, upload, and have the registry re-sign its snapshot:
#   trust_change <registry-url> <godspeed-trust namespaces arguments...>
trust_change() {
  local url=$1
  shift
  "$GODSPEED_TRUST" namespaces --dir "$TRUST_DIR" "$@" > /dev/null
  "$GODSPEED_TRUST" sign "$TRUST_DIR/namespaces.toml" --key "$TRUST_KEYS/namespaces.key" > /dev/null
  node scripts/admin.mjs upload-trust "$TRUST_DIR" --registry "$url" > /dev/null
}
