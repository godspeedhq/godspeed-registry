//! Who is asking, and may they publish here. Phase 1: hand-issued API tokens (`docs/publishing-design.md`
//! section 4 in the CLI repository).
//!
//! Only a token's SHA-256 is stored, so the database cannot leak a usable token. A token belongs to an
//! account; an account may publish under each namespace it is a member of. Authentication says nothing
//! about signing - that is a separate check, on a separate key.

use crate::api::{ApiError, ApiResult};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use worker::{D1Database, Request};

/// The authenticated caller.
#[derive(Debug, Clone)]
pub struct Caller {
    pub account: String,
    pub token_hash: String,
    pub expires: String,
}

#[derive(Deserialize)]
struct TokenRow {
    account: String,
    expires: String,
    revoked: Option<String>,
}

pub fn hash_token(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The bearer token from `Authorization: Bearer <token>`, if well formed.
pub fn bearer(req: &Request) -> Option<String> {
    let h = req.headers().get("Authorization").ok().flatten()?;
    let t = h.strip_prefix("Bearer ")?.trim();
    (t.starts_with("gsp_") && t.len() > 4).then(|| t.to_string())
}

/// Authenticate the request. `now` is the current time as `YYYY-MM-DDTHH:MM:SSZ`, which compares
/// correctly as a string.
pub async fn authenticate(db: &D1Database, req: &Request, now: &str) -> worker::Result<ApiResult<Caller>> {
    let Some(token) = bearer(req) else {
        return Ok(Err(ApiError::unauthenticated(
            "No publisher token. Run `godspeed login` with the token you were issued.",
        )));
    };
    let token_hash = hash_token(&token);
    let row: Option<TokenRow> = db
        .prepare("SELECT account, expires, revoked FROM tokens WHERE hash = ?1")
        .bind(&[token_hash.clone().into()])?
        .first(None)
        .await?;
    let Some(row) = row else {
        return Ok(Err(ApiError::unauthenticated(
            "This token is not one the registry issued.",
        )));
    };
    if let Some(when) = row.revoked {
        return Ok(Err(ApiError::unauthenticated(format!(
            "This token was revoked at {when}."
        ))));
    }
    if row.expires.as_str() <= now {
        return Ok(Err(ApiError::token_expired(format!(
            "This token expired at {}. Ask the registry maintainer for a new one.",
            row.expires
        ))));
    }
    Ok(Ok(Caller {
        account: row.account,
        token_hash,
        expires: row.expires,
    }))
}

/// May `account` publish under `publisher`?
pub async fn may_publish(db: &D1Database, account: &str, publisher: &str) -> worker::Result<bool> {
    #[derive(Deserialize)]
    struct One {
        #[allow(dead_code)]
        one: i64,
    }
    let row: Option<One> = db
        .prepare("SELECT 1 AS one FROM members WHERE publisher = ?1 AND account = ?2")
        .bind(&[publisher.into(), account.into()])?
        .first(None)
        .await?;
    Ok(row.is_some())
}

/// Is `key` registered, and not revoked, for signing releases under `publisher`?
pub async fn key_registered(db: &D1Database, publisher: &str, key: &str) -> worker::Result<bool> {
    #[derive(Deserialize)]
    struct One {
        #[allow(dead_code)]
        one: i64,
    }
    let row: Option<One> = db
        .prepare("SELECT 1 AS one FROM signing_keys WHERE publisher = ?1 AND key = ?2 AND revoked IS NULL")
        .bind(&[publisher.into(), key.into()])?
        .first(None)
        .await?;
    Ok(row.is_some())
}

/// The namespaces `account` may publish under.
pub async fn namespaces(db: &D1Database, account: &str) -> worker::Result<Vec<String>> {
    #[derive(Deserialize)]
    struct Row {
        publisher: String,
    }
    let rows = db
        .prepare("SELECT publisher FROM members WHERE account = ?1 ORDER BY publisher")
        .bind(&[account.into()])?
        .all()
        .await?
        .results::<Row>()?;
    Ok(rows.into_iter().map(|r| r.publisher).collect())
}

/// Revoke the caller's own token.
pub async fn revoke(db: &D1Database, token_hash: &str, now: &str) -> worker::Result<()> {
    db.prepare("UPDATE tokens SET revoked = ?1 WHERE hash = ?2 AND revoked IS NULL")
        .bind(&[now.into(), token_hash.into()])?
        .run()
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::hash_token;

    #[test]
    fn token_hash_is_sha256_hex() {
        // SHA-256("abc"), FIPS 180-2.
        assert_eq!(
            hash_token("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
