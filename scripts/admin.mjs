#!/usr/bin/env node
// Registry administration, phase 1: approve publishers, issue tokens and register signing keys by hand.
//
//   node scripts/admin.mjs add-publisher <publisher> <account>   [--remote]
//   node scripts/admin.mjs issue-token   <account> [--days 90]   [--remote]
//   node scripts/admin.mjs revoke-tokens <account>               [--remote]
//   node scripts/admin.mjs add-key       <publisher> <ed25519:...> [--remote]
//   node scripts/admin.mjs revoke-key    <publisher> <ed25519:...> [--remote]
//   node scripts/admin.mjs list                                  [--remote]
//
// Run `npm install` first (it pins wrangler). Without --remote it acts on the local development database
// that `wrangler dev` uses. A token is printed once and never stored: the database keeps only its SHA-256.
// A signing key is the PUBLIC key a publisher sends (`godspeed key show`); never accept a key file.

import { createHash, randomBytes } from "node:crypto";
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const args = process.argv.slice(2);
const remote = args.includes("--remote");
const daysAt = args.indexOf("--days");
const days = daysAt >= 0 ? Number(args[daysAt + 1]) : 90;
const positional = args.filter((a, i) => !a.startsWith("--") && !(daysAt >= 0 && i === daysAt + 1));
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

const KEY = /^ed25519:[0-9a-f]{64}$/; // the package contract's spelling of a public key

function key(value) {
  if (!value || !KEY.test(value)) fail(`"${value ?? ""}" is not a public key: expected ed25519: and 64 lowercase hex digits (from \`godspeed key show\`)`);
  return value; // safe to interpolate: the pattern admits no quote or SQL syntax
}

const iso = (d) => d.toISOString().replace(/\.\d{3}Z$/, "Z");

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const WRANGLER = join(ROOT, "node_modules", "wrangler", "bin", "wrangler.js");

// Statements go through a file, not the command line, so no shell ever re-splits or re-quotes them.
function sql(statement) {
  const dir = mkdtempSync(join(tmpdir(), "godspeed-admin-"));
  const file = join(dir, "statement.sql");
  writeFileSync(file, statement);
  try {
    // The pinned wrangler (package.json), run by this same Node: no shell, no PATH lookup, any OS.
    const out = execFileSync(
      process.execPath,
      [WRANGLER, "d1", "execute", "godspeed-registry", remote ? "--remote" : "--local", "--json", "--file", file],
      { encoding: "utf8", stdio: ["ignore", "pipe", "inherit"], cwd: ROOT },
    );
    return JSON.parse(out);
  } finally {
    rmSync(dir, { recursive: true, force: true });
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
    break;
  }
  case "issue-token": {
    const account = name(rest[0], "account");
    if (!Number.isInteger(days) || days < 1 || days > 366) fail("--days is a whole number from 1 to 366");
    const token = "gsp_" + randomBytes(32).toString("hex");
    const hash = createHash("sha256").update(token).digest("hex");
    const expires = iso(new Date(Date.now() + days * 86_400_000));
    const found = sql(`SELECT name FROM accounts WHERE name = '${account}'`)[0].results;
    if (found.length === 0) fail(`no account ${account}; add it with add-publisher first`);
    sql(`INSERT INTO tokens (hash, account, created, expires) VALUES ('${hash}', '${account}', '${now}', '${expires}')`);
    console.log(`Token for ${account}, expiring ${expires}. It is shown once; the registry keeps only its hash.`);
    console.log(token);
    break;
  }
  case "revoke-tokens": {
    const account = name(rest[0], "account");
    sql(`UPDATE tokens SET revoked = '${now}' WHERE account = '${account}' AND revoked IS NULL`);
    console.log(`Every token of ${account} is revoked.`);
    break;
  }
  case "add-key": {
    const publisher = name(rest[0], "publisher");
    const k = key(rest[1]);
    const found = sql(`SELECT name FROM publishers WHERE name = '${publisher}'`)[0].results;
    if (found.length === 0) fail(`no publisher ${publisher}; add it with add-publisher first`);
    sql(`INSERT OR IGNORE INTO signing_keys (publisher, key, added) VALUES ('${publisher}', '${k}', '${now}')`);
    const live = sql(`SELECT revoked FROM signing_keys WHERE publisher = '${publisher}' AND key = '${k}'`)[0].results[0];
    if (live.revoked) fail(`that key was revoked for ${publisher}/ at ${live.revoked}; a revoked key is never re-registered - ask for a new one`);
    console.log(`${k} may now sign releases under ${publisher}/`);
    break;
  }
  case "revoke-key": {
    const publisher = name(rest[0], "publisher");
    const k = key(rest[1]);
    sql(`UPDATE signing_keys SET revoked = '${now}' WHERE publisher = '${publisher}' AND key = '${k}' AND revoked IS NULL`);
    console.log(`${k} can no longer sign releases under ${publisher}/. Releases it already signed stay published.`);
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
    const keys = sql("SELECT publisher, key, added, revoked FROM signing_keys ORDER BY publisher, added")[0].results;
    for (const k of keys) console.log(`${k.publisher}/  key ${k.key}  ${k.revoked ? "revoked " + k.revoked : "since " + k.added}`);
    break;
  }
  default:
    fail(
      "commands: add-publisher <publisher> <account> | issue-token <account> [--days N] | revoke-tokens <account> | " +
        "add-key <publisher> <key> | revoke-key <publisher> <key> | list",
    );
}
