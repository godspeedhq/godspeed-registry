#!/usr/bin/env node
// Registry administration, phase 1: approve publishers and their tokens by hand, and upload the trust
// metadata signed offline with `godspeed-trust`. In production these run in the `admin` and `deploy`
// workflows, so the maintainer needs neither Node nor wrangler locally.
//
//   node scripts/admin.mjs add-publisher <publisher> <account>                  [--remote]
//   node scripts/admin.mjs approve-token <account> <fingerprint> [--days 90]    [--remote]
//   node scripts/admin.mjs revoke-tokens <account>                              [--remote]
//   node scripts/admin.mjs list                                                 [--remote]
//   node scripts/admin.mjs upload-trust  <trust-dir> [--registry <url>]         [--remote]
//
// Run `npm install` first (it pins wrangler). Without --remote it acts on the local development database and
// bucket that `wrangler dev` uses. Tokens are made by publishers (`godspeed login`); this only ever sees, and
// the database only ever holds, a token's SHA-256.
//
// Which keys may sign for a publisher is NOT kept here: it is the namespaces document, signed offline with
// `godspeed-trust` (tools/trust) on the machine that holds the offline keys. `upload-trust` only copies
// what that tool signed into the bucket - it signs nothing - and then asks the registry to re-sign its
// snapshot, so clients see the new document at once.

import { execFileSync } from "node:child_process";
import { existsSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const args = process.argv.slice(2);
const remote = args.includes("--remote");
const valueOf = (flag) => {
  const i = args.indexOf(flag);
  return i >= 0 ? args[i + 1] : undefined;
};
const days = valueOf("--days") !== undefined ? Number(valueOf("--days")) : 90;
const registry = valueOf("--registry");
const flagValues = new Set(["--days", "--registry"].map((f) => args.indexOf(f) + 1).filter((i) => i > 0));
const positional = args.filter((a, i) => !a.startsWith("--") && !flagValues.has(i));
const [command, ...rest] = positional;

const NAME = /^[a-z][a-z0-9-]{0,63}$/; // the package contract's rule for a publisher or service name

function fail(message) {
  console.error(`admin: ${message}`);
  process.exit(1);
}

function name(value, what) {
  if (!value || !NAME.test(value)) fail(`${what} "${value ?? ""}" must be a lowercase letter, then letters, digits and '-'`);
  return value; // safe to interpolate: the pattern admits no quote or SQL syntax
}

const iso = (d) => d.toISOString().replace(/\.\d{3}Z$/, "Z");

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const WRANGLER = join(ROOT, "node_modules", "wrangler", "bin", "wrangler.js");

// The pinned wrangler (package.json), run by this same Node: no shell, no PATH lookup, any OS.
function wrangler(argv, capture = true) {
  return execFileSync(process.execPath, [WRANGLER, ...argv], {
    encoding: "utf8",
    stdio: ["ignore", capture ? "pipe" : "inherit", "inherit"],
    cwd: ROOT,
  });
}

// A statement goes as one argument to the pinned wrangler, run with no shell (above), so nothing re-splits or
// re-quotes it; every value in one is checked by name() or is a hex digest, so none can carry SQL. Not
// --file: against the real database, that goes through D1's import route, which prints progress to stdout
// and returns no rows.
function sql(statement) {
  const out = wrangler(["d1", "execute", "godspeed-registry", remote ? "--remote" : "--local", "--json", "--command", statement]);
  try {
    return JSON.parse(out);
  } catch {
    fail(`wrangler did not answer in JSON:
${out}`);
  }
}

const now = iso(new Date());

switch (command) {
  case "add-publisher": {
    const publisher = name(rest[0], "publisher");
    const account = name(rest[1], "account");
    sql(
      `INSERT OR IGNORE INTO publishers (name, created) VALUES ('${publisher}', '${now}');` +
        `INSERT OR IGNORE INTO accounts (name, created) VALUES ('${account}', '${now}');` +
        `INSERT OR IGNORE INTO members (publisher, account) VALUES ('${publisher}', '${account}');`,
    );
    console.log(`${account} may now publish under ${publisher}/`);
    console.log(`Their signing key goes in the signed namespaces metadata: godspeed-trust namespaces add ${publisher} <key>`);
    break;
  }
  case "approve-token": {
    // The publisher made the token on their own machine (`godspeed login`) and sent only its fingerprint, the
    // SHA-256 the registry stores. Nothing secret reaches the maintainer, this script, or a workflow log.
    const account = name(rest[0], "account");
    const fingerprint = rest[1] ?? "";
    const m = /^(?:sha256:)?([0-9a-f]{64})$/.exec(fingerprint);
    if (!m) fail(`"${fingerprint}" is not a token fingerprint: expected sha256: and 64 lowercase hex digits (from \`godspeed login\`)`);
    const hash = m[1]; // safe to interpolate: the pattern admits only hex digits
    if (!Number.isInteger(days) || days < 1 || days > 366) fail("--days is a whole number from 1 to 366");
    const expires = iso(new Date(Date.now() + days * 86_400_000));
    const found = sql(`SELECT name FROM accounts WHERE name = '${account}'`)[0].results;
    if (found.length === 0) fail(`no account ${account}; add it with add-publisher first`);
    const existing = sql(`SELECT account FROM tokens WHERE hash = '${hash}'`)[0].results;
    if (existing.length > 0) fail(`that token is already approved, for ${existing[0].account}; a token is approved once`);
    sql(`INSERT INTO tokens (hash, account, created, expires) VALUES ('${hash}', '${account}', '${now}', '${expires}')`);
    console.log(`Approved sha256:${hash} for ${account}, until ${expires}.`);
    break;
  }
  case "revoke-tokens": {
    const account = name(rest[0], "account");
    sql(`UPDATE tokens SET revoked = '${now}' WHERE account = '${account}' AND revoked IS NULL`);
    console.log(`Every token of ${account} is revoked.`);
    break;
  }
  case "list": {
    const rows = sql(
      "SELECT m.publisher, m.account, COUNT(t.hash) AS live_tokens FROM members m " +
        `LEFT JOIN tokens t ON t.account = m.account AND t.revoked IS NULL AND t.expires > '${now}' ` +
        "GROUP BY m.publisher, m.account ORDER BY m.publisher, m.account",
    )[0].results;
    for (const r of rows) console.log(`${r.publisher}/  ${r.account}  (${r.live_tokens} live token(s))`);
    if (rows.length === 0) console.log("No publishers yet.");
    console.log("Signing keys: see the signed namespaces metadata (godspeed-trust verify).");
    break;
  }
  case "upload-trust": {
    const dir = rest[0] ?? fail("upload-trust <trust-dir>: the directory godspeed-trust wrote");
    // Checked before anything is uploaded, so a mistyped URL leaves nothing half done.
    if (registry && !/^https?:\/\/[^\s/]+/.test(registry)) {
      fail(`--registry ${JSON.stringify(registry)} is not a URL; it must start with https:// (in the workflows, check REGISTRY_URL in both the repository's variables and the production environment's: the environment's wins)`);
    }
    // Only the offline documents. Snapshot and timestamp are the registry's to sign, never uploaded.
    const files = [];
    for (const f of ["namespaces.toml", "namespaces.sig"]) if (existsSync(join(dir, f))) files.push(f);
    if (existsSync(join(dir, "root"))) {
      for (const f of readdirSync(join(dir, "root")).sort()) if (/^[1-9][0-9]*\.(toml|sig)$/.test(f)) files.push(`root/${f}`);
    }
    if (!files.some((f) => f.startsWith("root/"))) fail(`${dir} has no root/<n>.toml: run godspeed-trust root first`);
    for (const f of files) {
      wrangler(["r2", "object", "put", `godspeedos-packages/trust/${f}`, "--file", join(dir, f),
        "--content-type", "application/toml", remote ? "--remote" : "--local"]);
      console.log(`uploaded trust/${f}`);
    }
    if (registry) {
      const url = `${registry.replace(/\/$/, "")}/v1/trust/refresh`;
      const res = await fetch(url, { method: "POST" }).catch((e) =>
        fail(`could not reach ${url} (${e.cause?.code ?? e.message}); the files are uploaded, so run this again once it is reachable`));
      const body = await res.text();
      if (!res.ok) fail(`the registry did not re-sign its snapshot (HTTP ${res.status}): ${body}`);
      console.log(`registry re-signed: ${body}`);
    } else {
      console.log("Now ask the registry to re-sign its snapshot: POST <registry>/v1/trust/refresh (or pass --registry).");
    }
    break;
  }
  default:
    fail(
      "commands: add-publisher <publisher> <account> | approve-token <account> <fingerprint> [--days N] | revoke-tokens <account> | " +
        "list | upload-trust <trust-dir> [--registry <url>]",
    );
}
