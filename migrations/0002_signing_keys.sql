-- Publisher signing keys: which Ed25519 public keys may sign releases under which namespace.
--
-- Phase 1, and an interim record: the registry's own database is what binds a key to a namespace, so a
-- compromised registry could change it. The trust anchor that removes that weakness - root-signed metadata
-- listing each namespace's keys, which the registry cannot alter - is the next step
-- (docs/publishing-design.md section 6, in godspeed-cli). A key is never deleted: revoking it keeps the
-- record of what it was allowed to sign, and when that ended.
CREATE TABLE signing_keys (
    publisher TEXT NOT NULL REFERENCES publishers(name),
    key       TEXT NOT NULL,   -- ed25519:<64 lowercase hex digits>
    added     TEXT NOT NULL,
    revoked   TEXT,
    PRIMARY KEY (publisher, key)
);
