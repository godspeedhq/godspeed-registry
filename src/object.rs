//! One Durable Object per package: the authority on what is published and what is being published.
//!
//! Reservation and finalisation run inside `block_concurrency_while`, so for any one package exactly one of
//! them is in progress at a time, whatever the number of concurrent publishers. That is what makes "two
//! publishes of the same version: exactly one wins" hold without trusting the object store's conditional
//! writes alone.
//!
//! The commit point of a publish is the index write. Everything before it is safe to repeat - published
//! files are write-once and re-checked by digest - so a finalise that died half way is finished by the
//! next finalise of the same upload, never left half-visible.

use crate::api::{ApiError, ApiResult};
use crate::keys;
use crate::time::rfc3339;
use godspeed_package::{self as pkg, ArtifactVerifier, OwnedPackageIndex, PackageName, Sha256Digest, Version};
use serde::{Deserialize, Serialize};
use worker::*;

/// How long a reservation holds a version for its uploader.
pub const RESERVATION_MILLIS: u64 = 15 * 60 * 1000;

const RELEASES: &str = "releases";

/// `bytes` cryptographically random bytes, as lowercase hex, from the runtime's Web Crypto
/// (`crypto.getRandomValues`).
fn random_hex(bytes: u32) -> Result<String> {
    use wasm_bindgen::JsCast;
    let crypto = js_sys::Reflect::get(&js_sys::global(), &"crypto".into())?;
    let get: js_sys::Function = js_sys::Reflect::get(&crypto, &"getRandomValues".into())?
        .dyn_into()
        .map_err(|_| Error::RustError("crypto.getRandomValues is not available".into()))?;
    let buf = js_sys::Uint8Array::new_with_length(bytes);
    get.call1(&crypto, &buf)?;
    Ok(buf.to_vec().iter().map(|b| format!("{b:02x}")).collect())
}

fn reservation_key(version: &str) -> String {
    format!("reservation/{version}")
}

/// A published version, as the object remembers it. The index file is rendered from these.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredRelease {
    pub version: String,
    pub manifest: String,
    pub published: String,
    pub published_by: String,
}

/// A version held for one account's upload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reservation {
    pub id: String,
    pub version: String,
    /// The manifest's text, exactly as reserved: it is published byte for byte.
    pub manifest: String,
    pub manifest_digest: String,
    pub account: String,
    pub expires_ms: u64,
    /// Files received and verified so far.
    pub staged: Vec<String>,
    /// When it was published, once it has been.
    pub published: Option<String>,
}

impl Reservation {
    /// The files a release must upload: the contract, the publisher's signature, and the binaries the
    /// manifest names. (The manifest itself arrives with the reservation.)
    pub fn files(&self) -> Vec<String> {
        let owned = pkg::parse(&self.manifest).expect("validated at reservation");
        let m = owned.manifest();
        [pkg::CONTRACT_FILE.to_string(), pkg::SIGNATURE_FILE.to_string()]
            .into_iter()
            .chain(m.artifacts.iter().map(|a| a.arch.artifact_file().to_string()))
            .collect()
    }
}

#[derive(Serialize, Deserialize)]
pub struct ReserveRequest {
    pub name: String,
    pub manifest: String,
    pub account: String,
    pub now_ms: u64,
}

#[derive(Serialize, Deserialize)]
pub struct UploadRef {
    pub name: String,
    pub version: String,
    pub id: String,
    pub account: String,
    pub now_ms: u64,
    /// For `/staged` only: the file that was just stored and verified.
    pub file: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Published {
    pub name: String,
    pub version: String,
    pub manifest: String,
    pub published: String,
    /// The signed snapshot version that includes this release, or `None` if the catalog could not re-sign
    /// yet. The release is committed either way; clients see it once a snapshot lists it (the next refresh,
    /// at the latest within six hours).
    #[serde(default)]
    pub snapshot: Option<u64>,
}

/// Ask the catalog to re-sign the snapshot and timestamp, and report the snapshot version, if it could.
async fn refresh_snapshot(env: &Env, now_ms: u64) -> Option<u64> {
    let mut resp = crate::trust::request_refresh(env, now_ms).await.ok()?;
    if resp.status_code() != 200 {
        return None;
    }
    resp.json::<crate::trust::Refreshed>()
        .await
        .ok()
        .map(|r| r.snapshot_version)
}

/// Check that `bytes` are the file `file` that the manifest (`manifest_text`, parsed as `manifest`) names.
/// For the signature, that means it verifies over the manifest's exact bytes; it returns the key that signed,
/// which the caller must then find registered for the namespace.
pub fn check_file(
    manifest_text: &str,
    manifest: &pkg::Manifest<'_>,
    file: &str,
    bytes: &[u8],
) -> ApiResult<Option<pkg::PublicKey>> {
    if file == pkg::SIGNATURE_FILE {
        let text =
            std::str::from_utf8(bytes).map_err(|_| ApiError::signature_invalid(format!("{file} is not text")))?;
        let sig = pkg::ManifestSignature::parse(text)
            .and_then(|s| s.verify(manifest_text.as_bytes()).map(|()| s))
            .map_err(|e| ApiError::signature_invalid(format!("{file}: {e}")))?;
        return Ok(Some(sig.key));
    }
    if file == pkg::CONTRACT_FILE {
        let actual = Sha256Digest::of(bytes);
        if actual != manifest.contract {
            return Err(ApiError::digest_mismatch(format!(
                "{file}: expected {}, got {actual}",
                manifest.contract
            )));
        }
        // The right bytes, but are they the right contract? The same rule `godspeed verify` applies.
        let contract: toml::Table = std::str::from_utf8(bytes)
            .ok()
            .and_then(|t| t.parse().ok())
            .ok_or_else(|| ApiError::manifest_invalid(format!("{file} is not valid TOML")))?;
        let field = |k: &str| contract.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let mut problems = Vec::new();
        manifest.validate_against_contract(&field("name"), &field("version"), |p| problems.push(p.to_string()));
        return if problems.is_empty() {
            Ok(None)
        } else {
            Err(ApiError::manifest_invalid(problems.join("; ")))
        };
    }
    let Some(artifact) = manifest.artifacts.iter().find(|a| a.arch.artifact_file() == file) else {
        return Err(ApiError::file_not_in_manifest(format!(
            "{file} is not a file this package's manifest names"
        )));
    };
    ArtifactVerifier::verify(*artifact, bytes)
        .map(|()| None)
        .map_err(|e| match e {
            pkg::VerifyError::DigestMismatch { .. } => ApiError::digest_mismatch(format!("{file}: {e}")),
            _ => ApiError::size_mismatch(format!("{file}: {e}")),
        })
}

/// Look up a reservation for an upload, checking it is this account's, this upload's, and still open.
async fn open_reservation(storage: &Storage, r: &UploadRef) -> Result<ApiResult<Reservation>> {
    let Some(res) = storage.get::<Reservation>(&reservation_key(&r.version)).await? else {
        return Ok(Err(ApiError::not_found(format!(
            "No upload of {}@{} is in progress.",
            r.name, r.version
        ))));
    };
    if res.id != r.id {
        return Ok(Err(ApiError::not_found(format!(
            "Upload {} of {}@{} is not the one in progress.",
            r.id, r.name, r.version
        ))));
    }
    if res.account != r.account {
        return Ok(Err(ApiError::namespace_not_yours(
            "This upload was reserved by another account.",
        )));
    }
    if res.published.is_none() && res.expires_ms <= r.now_ms {
        return Ok(Err(ApiError::reservation_expired(format!(
            "The reservation of {}@{} expired. Start the publish again.",
            r.name, r.version
        ))));
    }
    Ok(Ok(res))
}

async fn reserve(storage: Storage, req: ReserveRequest) -> Result<ApiResult<Reservation>> {
    let owned = match pkg::parse(&req.manifest) {
        Ok(m) => m,
        Err(e) => return Ok(Err(ApiError::manifest_invalid(e.to_string()))),
    };
    let m = owned.manifest();
    let version = m.version.to_string();
    let digest = Sha256Digest::of(req.manifest.as_bytes()).to_string();

    let releases: Vec<StoredRelease> = storage.get(RELEASES).await?.unwrap_or_default();
    if let Some(r) = releases.iter().find(|r| r.version == version) {
        let same = if r.manifest == digest {
            "the same"
        } else {
            "a different"
        };
        return Ok(Err(ApiError::version_exists(format!(
            "{}@{version} was already published on {} with {same} manifest. Published versions never change.",
            req.name, r.published
        ))));
    }
    if let Some(existing) = storage.get::<Reservation>(&reservation_key(&version)).await? {
        if existing.published.is_none() && existing.expires_ms > req.now_ms {
            if existing.account == req.account && existing.manifest_digest == digest {
                return Ok(Ok(existing)); // the same publish, resumed
            }
            return Ok(Err(ApiError::version_being_published(format!(
                "{}@{version} is being published by another upload. Exactly one can win; this one did not.",
                req.name
            ))));
        }
    }
    // Random, not derived: a staged file's key contains this id and must not be guessable (keys.rs).
    let id = random_hex(16)?;
    let res = Reservation {
        id,
        version: version.clone(),
        manifest: req.manifest,
        manifest_digest: digest,
        account: req.account,
        expires_ms: req.now_ms + RESERVATION_MILLIS,
        staged: Vec::new(),
        published: None,
    };
    storage.put(&reservation_key(&version), &res).await?;
    Ok(Ok(res))
}

async fn staged(storage: Storage, r: UploadRef) -> Result<ApiResult<Reservation>> {
    let mut res = match open_reservation(&storage, &r).await? {
        Ok(res) => res,
        Err(e) => return Ok(Err(e)),
    };
    let file = r.file.unwrap_or_default();
    if !res.staged.contains(&file) {
        res.staged.push(file);
        storage.put(&reservation_key(&res.version), &res).await?;
    }
    Ok(Ok(res))
}

/// Write `bytes` to `key` only if nothing is there; if something is, it must be the same bytes.
async fn write_once(
    bucket: &Bucket,
    key: &str,
    bytes: Vec<u8>,
    content_type: &str,
    cache: &str,
) -> Result<ApiResult<()>> {
    let digest = Sha256Digest::of(&bytes);
    let written = bucket
        .put(key, bytes)
        .http_metadata(HttpMetadata {
            content_type: Some(content_type.into()),
            cache_control: Some(cache.into()),
            ..Default::default()
        })
        .only_if(Conditional {
            etag_does_not_match: Some("*".into()),
            ..Default::default()
        })
        .execute()
        .await?;
    if written.is_some() {
        return Ok(Ok(()));
    }
    // Already there: a retried finalise. It must be byte-identical, or something is badly wrong.
    let existing = match bucket.get(key).execute().await? {
        Some(o) => match o.body() {
            Some(b) => b.bytes().await?,
            None => Vec::new(),
        },
        None => Vec::new(),
    };
    if Sha256Digest::of(&existing) == digest {
        Ok(Ok(()))
    } else {
        Ok(Err(ApiError::conflict(format!(
            "{key} is already published with different bytes; nothing was changed."
        ))))
    }
}

async fn finalise(storage: Storage, env: Env, r: UploadRef) -> Result<ApiResult<Published>> {
    let mut res = match open_reservation(&storage, &r).await? {
        Ok(res) => res,
        Err(e) => return Ok(Err(e)),
    };
    if let Some(published) = &res.published {
        // Finalising twice is answered, not repeated (the refresh is a no-op unless the snapshot is stale).
        return Ok(Ok(Published {
            name: r.name,
            version: res.version,
            manifest: res.manifest_digest,
            published: published.clone(),
            snapshot: refresh_snapshot(&env, r.now_ms).await,
        }));
    }
    let missing: Vec<String> = res.files().into_iter().filter(|f| !res.staged.contains(f)).collect();
    if !missing.is_empty() {
        return Ok(Err(ApiError::upload_incomplete(format!(
            "Not yet uploaded: {}.",
            missing.join(", ")
        ))));
    }

    let owned = pkg::parse(&res.manifest).expect("validated at reservation");
    let manifest = owned.manifest();
    let name = PackageName::parse(&r.name).map_err(|e| Error::RustError(e.to_string()))?;
    let version: Version = res
        .version
        .parse()
        .map_err(|e: pkg::VersionError| Error::RustError(e.to_string()))?;
    let staging = env.bucket("PACKAGES")?;
    let packages = env.bucket("PACKAGES")?;
    const IMMUTABLE: &str = "public, max-age=31536000, immutable";

    // Re-verify every staged file from storage, then publish it write-once. The manifest goes last.
    for file in res.files() {
        let key = keys::staged(&name, version, &res.id, &file);
        let Some(obj) = staging.get(&key).execute().await? else {
            return Ok(Err(ApiError::upload_incomplete(format!(
                "{file} was accepted but is no longer staged; upload it again."
            ))));
        };
        let bytes = match obj.body() {
            Some(b) => b.bytes().await?,
            None => Vec::new(),
        };
        match check_file(&res.manifest, &manifest, &file, &bytes) {
            Err(e) => return Ok(Err(e)),
            // Checked again here, not only at upload: the key may have been retired or revoked in between.
            Ok(Some(key)) => {
                if let Err(e) = crate::trust::may_sign(&packages, name.publisher(), &key, &rfc3339(r.now_ms)).await? {
                    return Ok(Err(e));
                }
            }
            Ok(None) => {}
        }
        let content_type = if file.ends_with(".toml") || file.ends_with(".sig") {
            "application/toml"
        } else {
            "application/octet-stream"
        };
        if let Err(e) = write_once(
            &packages,
            &keys::published(&name, version, &file),
            bytes,
            content_type,
            IMMUTABLE,
        )
        .await?
        {
            return Ok(Err(e));
        }
    }
    if let Err(e) = write_once(
        &packages,
        &keys::published(&name, version, keys::MANIFEST_FILE),
        res.manifest.clone().into_bytes(),
        "application/toml",
        IMMUTABLE,
    )
    .await?
    {
        return Ok(Err(e));
    }

    // Record the release, then render and write the index: the commit point.
    let mut releases: Vec<StoredRelease> = storage.get(RELEASES).await?.unwrap_or_default();
    if !releases.iter().any(|x| x.version == res.version) {
        // Never earlier than the previous release, so the index stays in publication order even if
        // this isolate's clock is behind the last one's.
        let mut when = rfc3339(r.now_ms);
        if let Some(last) = releases.last() {
            if last.published > when {
                when = last.published.clone();
            }
        }
        releases.push(StoredRelease {
            version: res.version.clone(),
            manifest: res.manifest_digest.clone(),
            published: when,
            published_by: res.account.clone(),
        });
        storage.put(RELEASES, &releases).await?;
    }
    let mut index = OwnedPackageIndex::empty(&name);
    for x in &releases {
        let v: Version = x
            .version
            .parse()
            .map_err(|e: pkg::VersionError| Error::RustError(e.to_string()))?;
        let d: Sha256Digest = x
            .manifest
            .parse()
            .map_err(|e: pkg::DigestError| Error::RustError(e.to_string()))?;
        index.push(v, d, &x.published, &x.published_by);
    }
    let rs = index.releases();
    let text = pkg::index_to_toml(&index.index(&rs));
    packages
        .put(keys::index(&name), text)
        .http_metadata(HttpMetadata {
            content_type: Some("application/toml".into()),
            cache_control: Some("public, max-age=60".into()),
            ..Default::default()
        })
        .execute()
        .await?;

    let published = releases
        .iter()
        .find(|x| x.version == res.version)
        .map(|x| x.published.clone())
        .unwrap_or_default();
    res.published = Some(published.clone());
    storage.put(&reservation_key(&res.version), &res).await?;

    // Best effort: the lifecycle rule on `staging/` removes anything left behind.
    for file in res.files() {
        let _ = staging.delete(keys::staged(&name, version, &res.id, &file)).await;
    }
    Ok(Ok(Published {
        name: r.name,
        version: res.version,
        manifest: res.manifest_digest,
        published,
        snapshot: refresh_snapshot(&env, r.now_ms).await,
    }))
}

#[durable_object]
pub struct PackageObject {
    state: State,
    env: Env,
}

impl DurableObject for PackageObject {
    fn new(state: State, env: Env) -> Self {
        PackageObject { state, env }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let path = req.path();
        match path.as_str() {
            "/reserve" => {
                let body: ReserveRequest = req.json().await?;
                let storage = self.state.storage();
                crate::api::respond(self.state.block_concurrency_while(reserve(storage, body)).await)
            }
            "/reservation" => {
                let body: UploadRef = req.json().await?;
                crate::api::respond(open_reservation(&self.state.storage(), &body).await)
            }
            "/staged" => {
                let body: UploadRef = req.json().await?;
                let storage = self.state.storage();
                crate::api::respond(self.state.block_concurrency_while(staged(storage, body)).await)
            }
            "/finalise" => {
                let body: UploadRef = req.json().await?;
                let (storage, env) = (self.state.storage(), self.env.clone());
                crate::api::respond(self.state.block_concurrency_while(finalise(storage, env, body)).await)
            }
            _ => ApiError::not_found(format!("no such operation: {path}")).into_response(),
        }
    }
}
