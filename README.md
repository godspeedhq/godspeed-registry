# godspeed-registry

The Godspeed package registry service: the server `godspeed publish` uploads to, and the signed trust metadata
`godspeed fetch` (and, later, a Godspeed machine's package manager) verifies before keeping anything. A
Cloudflare Worker in front of R2. The design - and the reasons for each part of it - is
`docs/publishing-design.md` in [godspeed-cli](https://github.com/godspeedhq/godspeed-cli); the trust formats
are its `docs/trust-metadata.md`.

**Status: works end to end locally - publishing, publisher signatures, and the trust anchor. Not deployed.**

## What it does

    POST /v1/packages/<publisher>/<name>/reservations              body: manifest.toml
    PUT  /v1/packages/<publisher>/<name>/<version>/uploads/<id>/<file>
    POST /v1/packages/<publisher>/<name>/<version>/uploads/<id>/finalise
    GET  /v1/whoami
    POST /v1/logout                                                revokes the token used
    POST /v1/trust/refresh                                         re-sign snapshot/timestamp if needed (idempotent)

    GET  /index/<publisher>/<name>.toml                            the package index (public)
    GET  /packages/<publisher>/<name>/<version>/<file>             a published file (public)
    GET  /trust/{root/<n>,namespaces,snapshot,timestamp}.{toml,sig} the trust metadata (public)

- **No one holds storage credentials.** The Worker holds the bucket binding; publishers hold a token.
- **Every byte is verified** with `godspeed-package`, the same crate the CLI and the native package manager
  use: the manifest on reservation, each file as it is uploaded, and every file again at finalisation.
- **Every release is signed** (`manifest.sig`) by a key the signed namespaces metadata lists as active for
  its namespace; checked on upload and again at finalisation.
- **Which keys may sign for whom is signed offline** (`tools/trust`) and uploaded; the Worker verifies the
  root chain and the namespaces signatures exactly as a client does, and cannot change either.
- **The Worker signs snapshot and timestamp** with its one online key: the snapshot rebuilt from the bucket's
  actual index files after each publish, the timestamp valid a day and re-signed every six hours by a cron.
- **Uploads in progress** live under `staging/` in the same bucket for now, at random, unlisted, unindexed
  keys swept within a day; they move to a private bucket before anyone else can publish.
- **One Durable Object per package** serialises reservation and finalisation, so two publishes of one
  version cannot both win; **one catalog object** alone signs snapshots and timestamps, so their versions
  only increase.
- **The index write is the commit point.** Before it, nothing is visible; published files are write-once.
- Errors are JSON, `{"error": "<code>", "message": "..."}`, with stable codes (`src/api.rs`).

Reading needs none of this: in production the bucket's own domain serves `/index/`, `/packages/` and `/trust/`
directly, so downloads - and their verification - keep working when this Worker does not.

## Layout

    src/lib.rs          routes, authentication and authorisation, upload checks, the read routes, the cron
    src/object.rs       the per-package Durable Object: reserve, stage, finalise
    src/trust.rs        reading the offline trust metadata; the catalog that signs snapshot and timestamp
    src/auth.rs         tokens (stored as SHA-256 only) and namespace membership, in D1
    src/keys.rs         where everything lives in the bucket
    migrations/         the D1 schema
    tools/trust/        godspeed-trust: the maintainer's OFFLINE tool for root and namespaces
    scripts/admin.mjs   publishers, token approvals and upload-trust (run by the admin and deploy workflows)
    scripts/smoke.sh    the protocol end to end against a running registry
    scripts/local-test.sh   all of it from nothing, locally
    scripts/cli-test.sh     the real `godspeed` CLI against it, locally
    scripts/trust-setup.sh  a local trust anchor, for the two tests above

It depends on `godspeed-package` by path (`../godspeed-cli/crates/godspeed-package`), so check the two
repositories out side by side. That becomes a git dependency once the crate is on `godspeed-cli`'s main
branch.

## Local development

    npm install          # pins wrangler; approves only esbuild's and workerd's install scripts
    rustup target add wasm32-unknown-unknown
    scripts/local-test.sh <package-dir> path/to/godspeed
    GODSPEED_OS=<GodspeedOS checkout> scripts/cli-test.sh path/to/godspeed

Both wipe `.wrangler/state`, give the local registry a trust anchor made with `godspeed-trust` (offline keys,
root 1, namespaces), put its online key in `.dev.vars`, and start `wrangler dev` with every binding local.
Nothing reaches Cloudflare.

- `local-test.sh` runs `scripts/smoke.sh`: **33 checks**, from refused tokens and signatures to a concurrent
  reservation race and `godspeed fetch` verifying a release through the whole chain (and refusing a root not
  the one pinned).
- `cli-test.sh` drives the developer's side: **26 checks**, from making a token and having its fingerprint
  approved, through an unlisted signing key, to publish,
  `fetch`, a key retired (its earlier release still verifies; it signs nothing new; a new key takes over), a
  key revoked (what it signed is refused), and logout.

## Deploying (maintainer)

Deploys run in GitHub Actions (`.github/workflows/deploy.yml`), so no maintainer needs Node or wrangler on
their own machine: only Rust, for `godspeed-trust`. Not done yet. Once, in order:

1. **Offline keys**, on a machine you trust and ideally keep offline:
   `cargo run -p godspeed-trust -- keygen root-1.key` (and root-2, root-3, the namespaces keys, and one online
   key). Back each up; keep the root keys apart from one another. `.gitignore` refuses `*.key`, and CI fails
   if one is ever committed.
2. **Root 1**: `godspeed-trust root --expires <a year or more> --role root=2:<k1>,<k2>,<k3> --role
   namespaces=... --role snapshot=1:<online> --role timestamp=1:<online>`, then `godspeed-trust sign
   trust/root/1.toml --key ...` with enough root keys. Publish root 1's SHA-256 somewhere independent of the
   registry, so clients can pin it (`godspeed fetch --trust-root`).
3. **Namespaces**: `godspeed-trust namespaces add <publisher> <their public key>` (they get it from
   `godspeed key generate`), then `sign` with the namespaces keys; `godspeed-trust verify`.
4. **Commit `trust/`** (the signed files only) in a pull request. CI verifies it as a client would; the deploy
   uploads it.
5. In Cloudflare: create the D1 database `godspeed-registry` and put its id in `wrangler.toml`; add a lifecycle
   rule to `godspeedos-packages` deleting objects under `staging/` after one day; make an API token with
   Workers Scripts, D1 and Workers R2 Storage edit rights on this account.
6. In this repository's settings, an environment `production` with secrets `CLOUDFLARE_API_TOKEN`,
   `CLOUDFLARE_ACCOUNT_ID` and `ONLINE_SIGNING_KEY` (the `secret` line of the online key file - the only key
   that ever leaves your machine, because the registry must hold it), and variables `REGISTRY_DEPLOY =
   enabled` and `REGISTRY_URL` (the Worker's URL).
7. Merge to `main`, or run the workflow by hand: it applies migrations, deploys, sets the online key, uploads
   `trust/` and has the registry re-sign its snapshot.
8. Point the public bucket's custom domain at it for reads. The r2.dev URL is for development only.

From then on, everything is a pull request or a button in the Actions tab:

- **A new publisher:** they run `godspeed login --registry <url>` and send you the fingerprint it prints, with
  their account name. Run the **admin** workflow: `add-publisher` (publisher, account), then `approve-token`
  (account, fingerprint, days). Their token was made on their machine and never reaches you - only its
  fingerprint does, and that is all the registry stores, so nothing secret appears in any log.
- **A key change:** `godspeed-trust namespaces add | retire | revoke`, `sign`, commit `trust/`, merge.
- **Revoking someone's tokens:** the admin workflow's `revoke-tokens`.

Re-sign `namespaces` before it expires (a year by default) and `root` before its expiry; publishing and
fetching stop when either lapses, rather than continuing on expired trust.
