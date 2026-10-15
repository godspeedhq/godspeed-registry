#!/usr/bin/env bash
# End-to-end test of the publishing protocol against a running registry - `wrangler dev` locally, never
# production. Publishes one real package and checks every refusal along the way.
#
#   REGISTRY=http://127.0.0.1:8787 ALICE_TOKEN=gsp_... BOB_TOKEN=gsp_... \
#     scripts/smoke.sh <package-dir> [path-to-godspeed-cli-binary]
#
# <package-dir> is what `godspeed package` wrote for alice/<name>, at a version not yet published. With the
# CLI binary, the downloaded package is also checked with `godspeed verify`. BOB_TOKEN is revoked at the end.
set -u

REGISTRY=${REGISTRY:-http://127.0.0.1:8787}
PKG=${1:?usage: smoke.sh <package-dir> [godspeed-binary]}
GODSPEED=${2:-}
: "${ALICE_TOKEN:?set ALICE_TOKEN}" "${BOB_TOKEN:?set BOB_TOKEN}"

pass=0
fail=0
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

json() { python -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1], {}, {'d': d}))" "$1"; }
field() { sed -n "s/^$1 = \"\(.*\)\"$/\1/p" "$PKG/manifest.toml" | head -1; }
NAME=$(field name)
VERSION=$(field version)
PUBLISHER=${NAME%%/*}
SERVICE=${NAME#*/}
API="$REGISTRY/v1/packages/$NAME"

# call METHOD URL TOKEN [curl args...] -> sets STATUS and BODY
call() {
  local method=$1 url=$2 token=$3
  shift 3
  local auth=()
  [ -n "$token" ] && auth=(-H "Authorization: Bearer $token")
  BODY=$(curl -s -o "$WORK/body" -w "%{http_code}" -X "$method" "${auth[@]}" "$@" "$url"; echo)
  STATUS=${BODY%$'\n'}
  BODY=$(cat "$WORK/body")
}

# expect STATUS [ERROR-CODE] DESCRIPTION
expect() {
  local want=$1 code=$2 what=$3 got_code=""
  [ "$want" != 200 ] && got_code=$(printf '%s' "$BODY" | json "d.get('error','')" 2>/dev/null)
  if [ "$STATUS" = "$want" ] && { [ -z "$code" ] || [ "$got_code" = "$code" ]; }; then
    pass=$((pass + 1))
    echo "ok    $what"
  else
    fail=$((fail + 1))
    echo "FAIL  $what: wanted $want ${code}, got $STATUS: $BODY"
  fi
}

echo "Publishing $NAME@$VERSION to $REGISTRY"

call GET "$REGISTRY/v1/whoami" ""
expect 401 unauthenticated "no token is refused"
call GET "$REGISTRY/v1/whoami" "gsp_not-a-real-token"
expect 401 unauthenticated "an unknown token is refused"
call GET "$REGISTRY/v1/whoami" "$ALICE_TOKEN"
expect 200 "" "whoami names the account"
[ "$(printf '%s' "$BODY" | json "d['publishers']")" = "['$PUBLISHER']" ] || { fail=$((fail + 1)); echo "FAIL  whoami publishers: $BODY"; }

call POST "$API/reservations" "$BOB_TOKEN" --data-binary "@$PKG/manifest.toml"
expect 403 namespace-not-yours "bob cannot publish under $PUBLISHER/"
call POST "$API/reservations" "$ALICE_TOKEN" --data-binary "not = [toml"
expect 400 manifest-invalid "a broken manifest is refused"

call POST "$API/reservations" "$ALICE_TOKEN" --data-binary "@$PKG/manifest.toml"
expect 200 "" "alice reserves $VERSION"
UPLOAD=$(printf '%s' "$BODY" | json "d['upload']")
FILES=$(printf '%s' "$BODY" | json "' '.join(d['files'])")
# Staged keys contain the upload id and live in the public bucket for now, so it must be 128 random bits.
if printf '%s' "$UPLOAD" | grep -qE '^[0-9a-f]{32}$'; then
  pass=$((pass + 1)); echo "ok    the upload id is 32 hex digits (128 bits)"
else
  fail=$((fail + 1)); echo "FAIL  upload id '$UPLOAD' is not 128 bits of hex"
fi
call POST "$API/reservations" "$ALICE_TOKEN" --data-binary "@$PKG/manifest.toml"
expect 200 "" "reserving again resumes the same upload"
[ "$(printf '%s' "$BODY" | json "d['upload']")" = "$UPLOAD" ] || { fail=$((fail + 1)); echo "FAIL  resumed upload id differs"; }

UP="$API/$VERSION/uploads/$UPLOAD"
call POST "$UP/finalise" "$ALICE_TOKEN"
expect 409 upload-incomplete "finalising before every file is uploaded is refused"

printf 'name = "%s"\n' "$SERVICE" > "$WORK/fake-contract.toml"
call PUT "$UP/contract.toml" "$ALICE_TOKEN" --data-binary "@$WORK/fake-contract.toml"
expect 422 digest-mismatch "a contract that is not the manifest's is refused"
call PUT "$UP/notes.txt" "$ALICE_TOKEN" --data-binary "x"
expect 400 file-not-in-manifest "a file a package cannot contain is refused"
FIRST_ELF=$(printf '%s\n' $FILES | grep elf | head -1)
head -c 100 "$PKG/$FIRST_ELF" > "$WORK/short.elf"
call PUT "$UP/$FIRST_ELF" "$ALICE_TOKEN" --data-binary "@$WORK/short.elf"
expect 422 size-mismatch "a truncated binary is refused"
call PUT "$UP/$FIRST_ELF" "$BOB_TOKEN" --data-binary "@$PKG/$FIRST_ELF"
expect 403 namespace-not-yours "bob cannot upload into alice's reservation"

for f in $FILES; do
  call PUT "$UP/$f" "$ALICE_TOKEN" --data-binary "@$PKG/$f"
  expect 200 "" "upload $f"
done

call POST "$UP/finalise" "$ALICE_TOKEN"
expect 200 "" "finalise publishes $NAME@$VERSION"
PUBLISHED=$(printf '%s' "$BODY" | json "d['published']")
call POST "$UP/finalise" "$ALICE_TOKEN"
expect 200 "" "finalising again answers, and does not republish"
[ "$(printf '%s' "$BODY" | json "d['published']")" = "$PUBLISHED" ] || { fail=$((fail + 1)); echo "FAIL  second finalise changed the publication time"; }

call POST "$API/reservations" "$ALICE_TOKEN" --data-binary "@$PKG/manifest.toml"
expect 409 version-exists "a published version cannot be reserved again"

# The public read path: the index names the manifest by digest, and the files verify on their own.
call GET "$REGISTRY/index/$PUBLISHER/$SERVICE.toml" ""
expect 200 "" "the index is served"
WANT="sha256:$(sha256sum "$PKG/manifest.toml" | cut -d' ' -f1)"
if printf '%s' "$BODY" | grep -q "manifest = \"$WANT\""; then
  pass=$((pass + 1)); echo "ok    the index names the published manifest by its digest"
else
  fail=$((fail + 1)); echo "FAIL  index does not name $WANT: $BODY"
fi
mkdir -p "$WORK/download"
for f in manifest.toml $FILES; do
  curl -s -o "$WORK/download/$f" "$REGISTRY/packages/$NAME/$VERSION/$f"
done
if [ -n "$GODSPEED" ]; then
  if "$GODSPEED" verify "$WORK/download" > /dev/null; then
    pass=$((pass + 1)); echo "ok    the downloaded package passes godspeed verify"
  else
    fail=$((fail + 1)); echo "FAIL  the downloaded package does not verify"; "$GODSPEED" verify "$WORK/download"
  fi
fi

# Two publishes of one new version, at once: exactly one wins.
NEXT="${VERSION%.*}.$(( ${VERSION##*.} + 100 ))"
sed "s/^version = \"$VERSION\"$/version = \"$NEXT\"/" "$PKG/manifest.toml" > "$WORK/a.toml"
sed "s/^description = .*$/description = \"the other one\"/" "$WORK/a.toml" > "$WORK/b.toml"
for x in a b; do
  curl -s -o "$WORK/race-$x" -w "%{http_code}" -H "Authorization: Bearer $ALICE_TOKEN" \
    --data-binary "@$WORK/$x.toml" "$API/reservations" > "$WORK/race-$x.status" &
done
wait
# Each status file is a bare code with no newline, so join them explicitly.
STATUSES=$(printf '%s\n' "$(cat "$WORK/race-a.status")" "$(cat "$WORK/race-b.status")" | sort | tr '\n' ' ')
if [ "$STATUSES" = "200 409 " ]; then
  pass=$((pass + 1)); echo "ok    two reservations of $NEXT at once: exactly one won"
else
  fail=$((fail + 1)); echo "FAIL  race for $NEXT: statuses $STATUSES"
fi

call POST "$REGISTRY/v1/logout" "$BOB_TOKEN"
expect 200 "" "logout revokes the token"
call GET "$REGISTRY/v1/whoami" "$BOB_TOKEN"
expect 401 unauthenticated "a revoked token is refused"

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
