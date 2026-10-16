-- Phase 1 publishing authority: hand-approved namespaces and hand-issued tokens.
-- Ownership (members) and release attribution (the index's published_by) are kept separate.

-- A namespace packages are published under: `alice` in alice/hello.
CREATE TABLE publishers (
    name    TEXT PRIMARY KEY,
    created TEXT NOT NULL
);

-- Someone who can authenticate.
CREATE TABLE accounts (
    name    TEXT PRIMARY KEY,
    created TEXT NOT NULL
);

-- Which accounts may publish under which namespaces.
CREATE TABLE members (
    publisher TEXT NOT NULL REFERENCES publishers(name),
    account   TEXT NOT NULL REFERENCES accounts(name),
    PRIMARY KEY (publisher, account)
);

-- API tokens, by SHA-256 only: the database never holds a usable token.
-- Timestamps are YYYY-MM-DDTHH:MM:SSZ, which compare correctly as text.
CREATE TABLE tokens (
    hash    TEXT PRIMARY KEY,
    account TEXT NOT NULL REFERENCES accounts(name),
    created TEXT NOT NULL,
    expires TEXT NOT NULL,
    revoked TEXT
);
