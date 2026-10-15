# godspeed-registry

The publishing API of the Godspeed package registry: a Cloudflare Worker in front of R2. The design - and
the reasons for each part of it - is `docs/publishing-design.md` in
[godspeed-cli](https://github.com/godspeedhq/godspeed-cli).

**Status: works end to end locally, publisher signatures included. Not deployed.** The first public
publish waits for the trust anchor (signed root and index metadata - the design's section 6, slice 2):
until then, which key belongs to a publisher is this registry's own record, which clients cannot check
independently.

## What it does

    POST /v1/packages/<publisher>/<name>/reservations              body: manifest.toml
    PUT  /v1/packages/<publisher>/<name>/<version>/uploads/<id>/<file>
    POST /v1/packages/<publisher>/<name>/<version>/uploads/<id>/finalise
    GET  /v1/whoami
    POST /v1/logout                                                revokes the token used

    GET  /index/<publisher>/<name>.toml                            the package index (public)
    GET  /packages/<publisher>/<name>/<version>/<file>             a published file (public)

- **No one holds storage credentials.** The Worker holds the bucket bindings; publishers hold a token.
- **Every byte is verified** with `godspeed-package`, the same crate the CLI and the native package manager
  use: the manifest on reservation, each file as it is uploaded, and every file again at finalisation.
- **Every release is signed** (`manifest.sig`) by a key the maintainer registered for its namespace and has
  not revoked; checked on upload and again at finalisation, and published beside the manifest.
- **Uploads in progress** live under `staging/` in the same bucket for now, at random, unlisted, unindexed
  keys swept within a day; they move to a private bucket before anyone else can publish.
- **One Durable Object per package** serialises reservation and finalisation, so two publishes of one
  version cannot both win.
- **The index write is the commit point.** Before it, nothing is visible; published files are write-once
  and re-checked by digest, so a finalise that died half way is completed by the next one, never left
  half-visible.
- Errors are JSON, `{"error": "<code>", "message": "..."}`, with stable codes (`src/api.rs`).

Reading needs none of this: in production the bucket's own domain serves `/index/...` and `/packages/...`
directly, so downloads keep working when this Worker does not. The read routes here exist for local
development.

## Layout

    src/lib.rs        routes, authentication and authorisation, upload checks, the read routes
    src/object.rs     the per-package Durable Object: reserve, stage, finalise
    src/auth.rs       tokens (stored as SHA-256 only), namespace membership and signing keys, in D1
    src/keys.rs       where everything lives in the buckets
    migrations/       the D1 schema
    scripts/admin.mjs add publishers, issue and revoke tokens, register and revoke signing keys (by hand)
    scripts/smoke.sh  the protocol end to end against a running registry
    scripts/local-test.sh   all of it from nothing, locally
    scripts/cli-test.sh     the real `godspeed` CLI logging in and publishing to it, locally

It depends on `godspeed-package` by path (`../godspeed-cli/crates/godspeed-package`), so check the two
repositories out side by side. That becomes a git dependency once the crate is on `godspeed-cli`'s main
branch.

## Local development

    npm install          # pins wrangler; approves only esbuild's and workerd's install scripts
    rustup target add wasm32-unknown-unknown
    scripts/local-test.sh <package-dir> path/to/godspeed

`<package-dir>` is what `godspeed package` wrote for `alice/<name>`. The test wipes `.wrangler/state`,
creates publishers `alice` and `bob` with fresh tokens, a signing key registered for alice and a rogue one
registered for nobody, signs a copy of the package, starts `wrangler dev` (every binding local), and runs
`scripts/smoke.sh`: 30 checks, from refused tokens and signatures to a concurrent reservation race, ending
with the downloaded package passing `godspeed verify`. Nothing reaches Cloudflare.

    GODSPEED_OS=<GodspeedOS checkout> scripts/cli-test.sh path/to/godspeed

drives the developer's side instead: `login`, `whoami`, `key generate`, a publish refused for having no key
and then for an unregistered one, a publish that resumes once the key is registered, a second publish that
must report the identical release as already published, a refused release after the key is revoked, a
refused republish, a refused foreign namespace, and `logout`. 17 checks.

## Deploying (maintainer)

Not done yet. In order:

1. **Add a lifecycle rule to `godspeedos-packages`** deleting objects under the `staging/` prefix after one
   day; it sweeps abandoned uploads. (Uploads in progress share the public bucket for now - `CLAUDE.md`,
   invariant 5 - and move to a private bucket before anyone else is issued a token.)
2. `npx wrangler d1 create godspeed-registry`, and put the printed id in `wrangler.toml`.
3. `npx wrangler d1 migrations apply godspeed-registry --remote`
4. `npx wrangler deploy`
5. `node scripts/admin.mjs add-publisher <publisher> <account> --remote`, then
   `node scripts/admin.mjs issue-token <account> --remote`, and hand the token over privately. It is
   printed once; the registry keeps only its hash.
6. Register the publisher's signing key: they run `godspeed key generate` and send you the public key it
   prints (never a file); `node scripts/admin.mjs add-key <publisher> <ed25519:...> --remote`.
7. Point the public bucket's custom domain at it for reads. The r2.dev URL is for development only: it is
   rate-limited and uncached.
