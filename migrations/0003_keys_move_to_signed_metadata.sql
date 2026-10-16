-- Publisher signing keys now live in the signed namespaces metadata (trust/namespaces.toml), which the
-- maintainer signs offline and this registry cannot alter. A database row the registry could edit was the
-- weakness that metadata closes, so the table goes rather than being kept as a second, unsigned answer.
DROP TABLE signing_keys;
