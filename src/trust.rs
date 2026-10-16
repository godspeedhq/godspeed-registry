//! The registry's trust metadata, as the Worker uses it.
//!
//! The maintainer signs `root` and `namespaces` OFFLINE (`tools/trust`) and uploads them to `trust/`. This
//! Worker never holds those keys. It reads them - verifying the whole root chain and the namespaces
//! signatures exactly as a client does, so a publish is never accepted on the strength of metadata a
//! client would reject - and it decides with them which keys may sign a new release.
//!
//! It holds one ONLINE key (the `ONLINE_SIGNING_KEY` secret), listed in the root for the snapshot and
//! timestamp roles, and with it the catalog object (`Catalog`) keeps `trust/snapshot.toml` and
//! `trust/timestamp.toml` current: rebuilt from the bucket's actual index files, so they heal themselves even
//! after an update that failed half way.

use crate::api::{ApiError, ApiResult};
use crate::time::rfc3339;
use godspeed_package::trust::{self, Namespaces, Role, Root, Snapshot, Timestamp};
use godspeed_package::{PublicKey, Sha256Digest};
use serde::{Deserialize, Serialize};
use worker::*;

/// How long a snapshot is valid. It is re-signed well before then.
const SNAPSHOT_MILLIS: u64 = 7 * 86_400_000;
/// How long a timestamp is valid: a client is never shown a frozen registry for longer than this.
const TIMESTAMP_MILLIS: u64 = 86_400_000;
/// Re-sign the snapshot when this little of its life is left.
const SNAPSHOT_RENEW_MILLIS: u64 = 2 * 86_400_000;
/// Re-sign the timestamp when this little of its life is left. The cron runs every six hours.
const TIMESTAMP_RENEW_MILLIS: u64 = 12 * 3_600_000;

async fn text(bucket: &Bucket, key: &str) -> Result<Option<String>> {
    match bucket.get(key).execute().await? {
        None => Ok(None),
        Some(o) => match o.body() {
            Some(b) => Ok(Some(
                String::from_utf8(b.bytes().await?).map_err(|_| Error::RustError(format!("{key} is not UTF-8")))?,
            )),
            None => Ok(Some(String::new())),
        },
    }
}

/// The offline metadata, read from the bucket and verified.
pub struct Offline {
    /// The latest root's text.
    pub root: String,
    pub namespaces: String,
}

impl Offline {
    pub fn root(&self) -> Root<'_> {
        Root::parse(&self.root).expect("verified when loaded")
    }
    pub fn namespaces(&self) -> Namespaces<'_> {
        Namespaces::parse(&self.namespaces).expect("verified when loaded")
    }
}

fn not_initialised(why: String) -> ApiError {
    ApiError::trust_not_initialised(format!(
        "{why} Nothing can be published until the registry maintainer uploads signed trust metadata."
    ))
}

/// Load and verify the root chain (`trust/root/1.toml`, `2.toml`, ...) and `trust/namespaces.toml`.
pub async fn load_offline(bucket: &Bucket, now: &str) -> Result<ApiResult<Offline>> {
    let Some(first) = text(bucket, "trust/root/1.toml").await? else {
        return Ok(Err(not_initialised("The registry has no root metadata.".into())));
    };
    let first_sig = text(bucket, "trust/root/1.sig").await?.unwrap_or_default();
    let root1 = match Root::parse(&first) {
        Ok(r) => r,
        Err(e) => return Ok(Err(not_initialised(format!("Root 1 is invalid: {e}.")))),
    };
    if let Err(e) = trust::verify_by(&root1, Role::Root, first.as_bytes(), &first_sig) {
        return Ok(Err(not_initialised(format!("Root 1: {e}."))));
    }
    let mut current = first;
    let mut version = 1u64;
    loop {
        let key = format!("trust/root/{}.toml", version + 1);
        let Some(next) = text(bucket, &key).await? else { break };
        let sig = text(bucket, &format!("trust/root/{}.sig", version + 1))
            .await?
            .unwrap_or_default();
        let trusted = Root::parse(&current).expect("verified");
        if let Err(e) = trust::verify_next_root(&trusted, &next, &sig) {
            return Ok(Err(not_initialised(format!(
                "Root {} does not follow from root {version}: {e}.",
                version + 1
            ))));
        }
        current = next;
        version += 1;
    }
    let root = Root::parse(&current).expect("verified");
    if let Err(e) = root.header.check_fresh(now) {
        return Ok(Err(ApiError::trust_expired(format!(
            "Root {version}: {e}. The maintainer must sign a new root."
        ))));
    }
    let Some(ns) = text(bucket, "trust/namespaces.toml").await? else {
        return Ok(Err(not_initialised("The registry has no namespaces metadata.".into())));
    };
    let ns_sig = text(bucket, "trust/namespaces.sig").await?.unwrap_or_default();
    let parsed = match Namespaces::parse(&ns) {
        Ok(n) => n,
        Err(e) => {
            return Ok(Err(not_initialised(format!(
                "The namespaces metadata is invalid: {e}."
            ))))
        }
    };
    if let Err(e) = trust::verify_by(&root, Role::Namespaces, ns.as_bytes(), &ns_sig) {
        return Ok(Err(not_initialised(format!("The namespaces metadata: {e}."))));
    }
    if let Err(e) = parsed.header.check_fresh(now) {
        return Ok(Err(ApiError::trust_expired(format!(
            "{e}. The maintainer must sign a new namespaces document."
        ))));
    }
    Ok(Ok(Offline {
        root: current,
        namespaces: ns,
    }))
}

/// May `key` sign a NEW release under `publisher`? Only a key the signed namespaces list as active may.
pub async fn may_sign(bucket: &Bucket, publisher: &str, key: &PublicKey, now: &str) -> Result<ApiResult<()>> {
    let offline = match load_offline(bucket, now).await? {
        Ok(o) => o,
        Err(e) => return Ok(Err(e)),
    };
    if offline.namespaces().may_sign(publisher, key) {
        return Ok(Ok(()));
    }
    let why = match offline.namespaces().status(publisher, key) {
        Some(trust::KeyStatus::Retired(at)) => format!("{key} was retired for {publisher}/ at {at}."),
        Some(trust::KeyStatus::Revoked(at)) => format!("{key} was revoked for {publisher}/ at {at}."),
        _ => format!("{key} is not one of {publisher}/'s keys in the signed namespaces metadata."),
    };
    Ok(Err(ApiError::key_not_registered(format!(
        "{why} Releases under {publisher}/ must be signed by an active key the registry maintainer signed for."
    ))))
}

// ---- the catalog: snapshot and timestamp, with the online key ----------------------------------------------

fn online_seed(env: &Env) -> Result<[u8; 32]> {
    let hex = env.secret("ONLINE_SIGNING_KEY")?.to_string();
    let hex = hex.trim().as_bytes();
    let mut seed = [0u8; 32];
    if hex.len() != 64 {
        return Err(Error::RustError("ONLINE_SIGNING_KEY is not 64 hex digits".into()));
    }
    for (i, pair) in hex.as_chunks::<2>().0.iter().enumerate() {
        seed[i] = u8::from_str_radix(std::str::from_utf8(pair).unwrap_or("zz"), 16)
            .map_err(|_| Error::RustError("ONLINE_SIGNING_KEY is not hex".into()))?;
    }
    Ok(seed)
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Signed {
    snapshot_version: u64,
    snapshot_expires_ms: u64,
    timestamp_version: u64,
    timestamp_expires_ms: u64,
    /// What the current snapshot was made from, to tell whether anything changed.
    namespaces: Option<(u64, String)>,
    indexes: Vec<(String, String)>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Refreshed {
    pub snapshot_version: u64,
    pub timestamp_version: u64,
    pub snapshot_changed: bool,
    pub timestamp_changed: bool,
}

async fn put_toml(bucket: &Bucket, key: &str, body: String, max_age: u32) -> Result<()> {
    bucket
        .put(key, body)
        .http_metadata(HttpMetadata {
            content_type: Some("application/toml".into()),
            cache_control: Some(format!("public, max-age={max_age}")),
            ..Default::default()
        })
        .execute()
        .await?;
    Ok(())
}

/// Rebuild the snapshot from the bucket and re-sign what needs it. Idempotent: with nothing changed and
/// plenty of validity left it writes nothing, so anyone may trigger it.
async fn refresh(storage: Storage, env: Env, now_ms: u64) -> Result<ApiResult<Refreshed>> {
    let bucket = env.bucket("PACKAGES")?;
    let now = rfc3339(now_ms);
    let offline = match load_offline(&bucket, &now).await? {
        Ok(o) => o,
        Err(e) => return Ok(Err(e)),
    };
    let seed = online_seed(&env)?;
    let online: PublicKey = godspeed_package::ManifestSignature::public_key(&seed);
    let root = offline.root();
    for role in [Role::Snapshot, Role::Timestamp] {
        if !root.has_key(role, &online) {
            return Ok(Err(not_initialised(format!(
                "The registry's online key {online} is not a {role} key in the current root."
            ))));
        }
    }

    // What the snapshot must say now: the namespaces document, and every index exactly as stored.
    let ns_version = offline.namespaces().header.version;
    let ns_digest = Sha256Digest::of(offline.namespaces.as_bytes());
    let mut indexes = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut list = bucket.list().prefix("index/");
        if let Some(c) = &cursor {
            list = list.cursor(c.clone());
        }
        let page = list.execute().await?;
        for o in page.objects() {
            let key = o.key();
            let Some(name) = key.strip_prefix("index/").and_then(|k| k.strip_suffix(".toml")) else {
                continue;
            };
            let Some(body) = text(&bucket, &key).await? else {
                continue;
            };
            indexes.push((name.to_string(), Sha256Digest::of(body.as_bytes()).to_string()));
        }
        if !page.truncated() {
            break;
        }
        cursor = page.cursor();
    }
    indexes.sort();

    let mut signed: Signed = storage.get("signed").await?.unwrap_or_default();
    let inputs_changed =
        signed.namespaces.as_ref() != Some(&(ns_version, ns_digest.to_string())) || signed.indexes != indexes;
    let snapshot_changed =
        signed.snapshot_version == 0 || inputs_changed || signed.snapshot_expires_ms < now_ms + SNAPSHOT_RENEW_MILLIS;
    if snapshot_changed {
        signed.snapshot_version += 1;
        signed.snapshot_expires_ms = now_ms + SNAPSHOT_MILLIS;
        let list: Vec<(String, Sha256Digest)> = indexes
            .iter()
            .map(|(n, d)| (n.clone(), d.parse().expect("written by us")))
            .collect();
        let body = trust::write_snapshot(
            signed.snapshot_version,
            &rfc3339(signed.snapshot_expires_ms),
            (ns_version, ns_digest),
            &list,
        );
        Snapshot::parse(&body).map_err(|e| Error::RustError(format!("wrote an invalid snapshot: {e}")))?;
        let sig = trust::sign_document(Role::Snapshot, &body, &[seed]);
        // Signature first, document second: a reader that sees the new document finds its signature.
        put_toml(&bucket, "trust/snapshot.sig", sig, 60).await?;
        put_toml(&bucket, "trust/snapshot.toml", body, 60).await?;
        signed.namespaces = Some((ns_version, ns_digest.to_string()));
        signed.indexes = indexes;
    }
    let timestamp_changed = snapshot_changed || signed.timestamp_expires_ms < now_ms + TIMESTAMP_RENEW_MILLIS;
    if timestamp_changed {
        let snapshot_body = text(&bucket, "trust/snapshot.toml").await?.unwrap_or_default();
        signed.timestamp_version += 1;
        signed.timestamp_expires_ms = now_ms + TIMESTAMP_MILLIS;
        let body = trust::write_timestamp(
            signed.timestamp_version,
            &rfc3339(signed.timestamp_expires_ms),
            (signed.snapshot_version, Sha256Digest::of(snapshot_body.as_bytes())),
        );
        Timestamp::parse(&body).map_err(|e| Error::RustError(format!("wrote an invalid timestamp: {e}")))?;
        let sig = trust::sign_document(Role::Timestamp, &body, &[seed]);
        put_toml(&bucket, "trust/timestamp.sig", sig, 30).await?;
        put_toml(&bucket, "trust/timestamp.toml", body, 30).await?;
    }
    storage.put("signed", &signed).await?;
    Ok(Ok(Refreshed {
        snapshot_version: signed.snapshot_version,
        timestamp_version: signed.timestamp_version,
        snapshot_changed,
        timestamp_changed,
    }))
}

/// The single object that signs snapshots and timestamps, so versions only ever increase.
#[durable_object]
pub struct Catalog {
    state: State,
    env: Env,
}

impl DurableObject for Catalog {
    fn new(state: State, env: Env) -> Self {
        Catalog { state, env }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        #[derive(Deserialize)]
        struct Body {
            now_ms: u64,
        }
        let body: Body = req.json().await?;
        let (storage, env) = (self.state.storage(), self.env.clone());
        crate::api::respond(
            self.state
                .block_concurrency_while(refresh(storage, env, body.now_ms))
                .await,
        )
    }
}

/// Ask the catalog to refresh. The one instance is named "catalog".
pub async fn request_refresh(env: &Env, now_ms: u64) -> Result<Response> {
    let stub = env.durable_object("CATALOG")?.id_from_name("catalog")?.get_stub()?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_body(Some(wasm_bindgen::JsValue::from_str(&format!(
            "{{\"now_ms\":{now_ms}}}"
        ))));
    stub.fetch_with_request(Request::new_with_init("https://catalog/refresh", &init)?)
        .await
}
