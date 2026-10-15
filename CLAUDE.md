# Godspeed Registry - Skill Definition

## Purpose

Build and maintain the **Godspeed package registry service**: the server that `godspeed publish` uploads
packages to, and the storage layout that `godspeed install` (on a Godspeed machine) downloads them from.

It is to Godspeed packages what crates.io is to Rust crates. It is not the CLI, and it does not distribute
the CLI binary: `godspeed` itself is released from the `godspeed-cli` repository's GitHub Releases.

    godspeedhq/godspeed-os         the operating system
    godspeedhq/godspeed-cli        the developer tool, and the package contract crate (godspeed-package)
    godspeedhq/godspeed-registry   this: the service packages are published to

Concretely, this repository holds:

- a **Cloudflare Worker** - the publishing API: authenticate the publisher, check the namespace, verify
  every uploaded byte, and finalise a release atomically;
- the **D1 schema** for publishers, accounts, namespace membership and token hashes;
- the **admin tooling** for approving publishers and issuing tokens by hand (phase 1);
- the **deployment configuration** and the **tests** of the protocol.

The design it implements, and the reasons for it, are `docs/publishing-design.md` in `godspeed-cli`. Read
it before changing behaviour here. Where this file and that design disagree, stop and reconcile them; do not
quietly pick one.

---

## The Registry Distributes Software. It Decides Nothing About Authority.

A package's contract REQUESTS capabilities. The registry stores that contract byte for byte and never
interprets it as a grant. The operating system decides what installed software may do.

Never add anything to the registry that grants, ranks, recommends or approves what a package may do.

---

## Architecture

    developer: godspeed publish ----> this Worker (writes) ----> R2 godspeedos-packages  (PUBLIC, read-only)
                                                                    index/     packages/     staging/
                                                                        ^
    anyone: godspeed install, curl -------------------------------------+  (the bucket's own domain;
                                                                              never this Worker)

- **Reads never depend on this Worker.** The public bucket's domain serves `index/` and `packages/`
  directly, so downloads keep working when the publishing API is down. The read routes in this Worker exist
  for local development and as a fallback only.
- **Writes only go through this Worker.** No developer, CI job or CLI ever holds storage credentials. The
  CLI talks to the registry protocol, never to R2's API.
- **One Durable Object per package** is the authority on what is published and what is being published.
- **D1** holds who may publish where.

---

## Invariants

These hold after every change. A change that weakens one is wrong even if every test passes.

1. **A published version is immutable.** The same `publisher/name@version` is never replaced with different
   bytes. Published files are written once (conditional put) and a repeat write must be byte-identical.
   There is no delete or overwrite endpoint, and there must not be one.
2. **A partial or failed release is never visible.** The index write is the commit point: every file it
   names is already in place before it is written. Nothing in staging is ever served.
3. **Concurrent publishes of one version: exactly one wins**, and the other is told so. Reservation and
   finalisation for a package run serialised in its Durable Object (`block_concurrency_while`).
4. **Every byte is verified** with `godspeed-package` - the same crate the CLI and the native package
   manager use - on reservation (the manifest), on upload (each file), and again at finalisation (every file,
   read back from staging). Never write a second implementation of the manifest rules here.
5. **Nothing in staging is findable or installable.** For now uploads in progress live under the
   `staging/` prefix of the public bucket (the maintainer's choice, to run one bucket). R2 public access is
   per bucket, not per prefix, so this holds only while all of these do:
   - upload ids are 128 cryptographically random bits, so a staged key cannot be guessed, and the public
     URL does not list keys;
   - the index never names a staged file, so nothing in staging can be installed;
   - a lifecycle rule on `staging/` deletes what is left after a day;
   - once the custom domain exists, a rule on it refuses `/staging/*` (r2.dev cannot do this, which is one
     more reason to retire r2.dev then).

   **Before anyone but the maintainer holds a publisher token, staging moves to a separate private bucket**
   (`godspeedos-staging`, never given a public domain). Until then the only person who can know a staged
   key is the maintainer, who uploaded it.
6. **No one but the publisher ever holds their token.** Publishers make tokens on their own machines and send
   only the fingerprint (the SHA-256), which the maintainer approves; the database stores only that. Never
   add a way to issue, print, email or log a token on the registry's side.
7. **Every release is signed by a key the signed namespaces metadata lists as active for its namespace.**
   `manifest.sig` is a required file; it must verify over the reserved manifest's exact bytes, and its key
   must be active for the publisher in `trust/namespaces.toml` - which the Worker verifies, through the
   whole root chain, exactly as a client does - checked when it is uploaded and again at finalisation. Which
   keys may sign for whom is decided ONLY there, signed offline: never in the database, never by the Worker.
   A retired or revoked key signs nothing new; nothing already published is ever altered.
8. **The Worker signs only snapshot and timestamp, with the one online key, and only the catalog object
   signs them**, so their versions only increase. The snapshot is rebuilt from the bucket's actual index files,
   never from a cached list. The Worker never holds, uploads or signs root or namespaces.
9. **A timed-out or failed request is not proof of anything.** A storage or runtime error is reported as
   `registry-unavailable`, which the CLI must treat as an unknown outcome and reconcile by reading the index.
   Never report success before it is established, and never report a refusal for something that may have
   happened.

---

## Separate Concerns

Keep these five apart, in code, in errors and in documentation:

| concern | established by |
|---|---|
| content integrity | digests in the manifest, checked by `godspeed-package` |
| publisher identity | publisher signatures (`manifest.sig`), by keys the offline-signed namespaces metadata lists |
| registry authorisation | tokens and namespace membership (D1) |
| local capability grants | the operating system, never the registry |
| software safety | nobody: a verified package is not a safe package |

Authentication is not signing. A valid token says an account may publish under a namespace; it says nothing
about which key signed a release.

---

## The Protocol

    POST /v1/packages/<publisher>/<name>/reservations              body: manifest.toml
    PUT  /v1/packages/<publisher>/<name>/<version>/uploads/<id>/<file>
    POST /v1/packages/<publisher>/<name>/<version>/uploads/<id>/finalise
    GET  /v1/whoami
    POST /v1/logout

    GET  /index/<publisher>/<name>.toml
    GET  /packages/<publisher>/<name>/<version>/<file>

- Errors are `{"error": "<code>", "message": "..."}` with the codes in `src/api.rs`. **The codes are part of
  the protocol**: the CLI maps each to a diagnostic. Add codes freely; never rename or repurpose one within
  `/v1`.
- The bucket layout is spelled in one place, `src/keys.rs`. The package and index formats belong to
  `godspeed-package`, not here.
- Every refusal says what happened and what to do next, in the `message`, in words a developer can act on.

---

## Trust Model (Built)

The registry follows The Update Framework's roles - root, namespaces (delegated targets), snapshot,
timestamp - implemented directly, scoped to what the design needs, so the native package manager can verify
without a heap. Formats: `docs/trust-metadata.md` in `godspeed-cli`; the code is `godspeed-package`'s
`trust` module, shared by this Worker, `tools/trust` and the CLI. Do not adopt a TUF library that cannot run
`no_std`, and never write a second implementation of these checks here.

- **Root and namespaces are signed offline** by the maintainer with `tools/trust` and uploaded with
  `admin.mjs upload-trust`. A compromised registry cannot add or swap a publisher's key.
- **Retire and revoke are different.** Retire (rotation): the key signs nothing new; what it published before
  stays trusted. Revoke (compromise): nothing it signed is trusted.
- **Snapshot and timestamp are signed online** by the catalog (invariant 8). A compromised online key can
  delay or freeze what clients see for at most a timestamp's life (one day); it cannot forge a release.
- **The first public publish waits only on deployment**: offline keys made and kept apart, root 1 signed by
  its threshold and its digest published somewhere independent of the registry, the online key set as a
  secret. Until then nothing may imply the official registry is live.

---

## Dependencies on Other Repositories

- `godspeed-package` comes from `godspeed-cli`. Today it is a path dependency
  (`../godspeed-cli/crates/godspeed-package`); once that crate is on `godspeed-cli`'s main branch it becomes a
  git dependency at a pinned revision. A change the registry needs in the package format is made **there**,
  in `godspeed-cli`, never forked here.
- Nothing here depends on the OS repository, and nothing here may ask the OS to change for the registry's
  convenience.

---

## Testing

    npm install
    rustup target add wasm32-unknown-unknown
    scripts/local-test.sh <package-dir> path/to/godspeed

`local-test.sh` wipes local state, creates publishers, starts `wrangler dev` with every binding local, and
runs `scripts/smoke.sh`: the protocol end to end, including every refusal, a real publish verified by
`godspeed verify` after download, and a concurrent reservation race.

- Every new refusal gets a check in `smoke.sh`; every new invariant gets a check that would fail without it.
- Tests run against local bindings only. **Never point a test at the production buckets or database.**
- `scripts/cli-test.sh` drives the registry with the real `godspeed` CLI; a protocol change is not done until
  it passes too.
- The crate is `#![forbid(unsafe_code)]`: nothing here needs it, and `forbid` cannot be switched off locally.
- Host unit tests (`cargo test`) cover the pure parts: time formatting, key layout, token hashing, error
  codes.

---

## Operations

- Deploying, creating buckets and databases, and issuing tokens (`--remote`) act on the maintainer's
  Cloudflare account. **Do them only when the maintainer asks**, and say exactly what will change first.
- Publisher and token approval is by hand in phase 1, through the `admin` workflow (`scripts/admin.mjs` on
  GitHub's runner): nothing it handles is secret. Self-service registration can come later without changing
  the package format.
- Pin tooling exactly (`wrangler` in `package.json`, `worker-build` in `wrangler.toml`): a build of a given
  commit must use the same tools every time.
- **Offline keys never touch the registry**, a CI runner, or this repository. `tools/trust` runs on the
  maintainer's machine; only its signed output is uploaded. Never upload `snapshot` or `timestamp` by hand:
  they are the catalog's alone.
- `namespaces` and `root` expire; publishing and fetching stop when they do, by design. Re-sign them before
  that, and never extend an expiry to make a failure go away.

---

## What Not To Build

- a marketplace, storefront, ratings, reviews, likes, download counts or recommendations;
- accounts beyond what publishing needs; a web frontend; analytics or telemetry;
- delete or overwrite of a published version (yanking - "not for new installs" - is a separate, later
  design that leaves the bytes in place);
- anything that turns a package's requested capabilities into granted ones;
- R2-specific behaviour on the read path: a mirror on any static host must be able to serve the same files.

---

## Final Principle

The registry handles the routine mechanics of distribution, and is honest about the rest: it proves that
bytes are the ones a publisher uploaded, it does not yet prove who that publisher is, and it never decides
what a package may do.
