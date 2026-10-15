//! The Godspeed package registry's publishing API.
//!
//! A Cloudflare Worker in front of the R2 bucket `PACKAGES`: public and read-only to the world, with uploads
//! in progress under its `staging/` prefix (see `keys::staged` for why that is safe for now, and when it
//! must move to a private bucket). Developers never hold storage credentials: this Worker
//! holds the bindings, authenticates publishers, verifies every byte with the same `godspeed-package`
//! crate the CLI and the native package manager use, and finalises atomically through one Durable Object
//! per package. The design is `docs/publishing-design.md` in the `godspeed-cli` repository.
//!
//! Reading needs none of this. `GET /index/...` and `GET /packages/...` here serve the public bucket for
//! local development and as a fallback; in production the custom domain serves the bucket directly, so
//! downloads keep working when this Worker does not.

// Nothing here needs `unsafe`; `forbid` (unlike `deny`) cannot be switched off further in.
#![forbid(unsafe_code)]

mod api;
mod auth;
mod keys;
mod object;
mod time;

pub use object::PackageObject;

use api::{respond, ApiError, ApiResult};
use godspeed_package::{self as pkg, PackageName, Version};
use object::{Reservation, ReserveRequest, UploadRef};
use serde::Serialize;
use worker::*;

fn now_ms() -> u64 {
    Date::now().as_millis()
}

/// Ask a package's Durable Object to do `op`.
async fn call(env: &Env, name: &PackageName<'_>, op: &str, body: &impl Serialize) -> Result<Response> {
    let stub = env
        .durable_object("PACKAGE")?
        .id_from_name(&name.to_string())?
        .get_stub()?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_body(Some(wasm_bindgen::JsValue::from_str(&serde_json::to_string(body)?)));
    stub.fetch_with_request(Request::new_with_init(&format!("https://package{op}"), &init)?)
        .await
}

fn package_name<'a>(publisher: &str, name: &str, buf: &'a mut String) -> ApiResult<PackageName<'a>> {
    *buf = format!("{publisher}/{name}");
    PackageName::parse(buf)
        .map_err(|e| ApiError::not_found(format!("\"{publisher}/{name}\" is not a package name: {e}")))
}

fn param<'a>(ctx: &'a RouteContext<()>, key: &str) -> &'a str {
    ctx.param(key).map(String::as_str).unwrap_or_default()
}

#[derive(Serialize)]
struct WhoAmI {
    account: String,
    publishers: Vec<String>,
    token_expires: String,
}

/// What a client sees of a reservation.
#[derive(Serialize)]
struct ReservationOut {
    upload: String,
    version: String,
    expires: String,
    files: Vec<String>,
    staged: Vec<String>,
}

impl From<&Reservation> for ReservationOut {
    fn from(r: &Reservation) -> Self {
        ReservationOut {
            upload: r.id.clone(),
            version: r.version.clone(),
            expires: time::rfc3339(r.expires_ms),
            files: r.files(),
            staged: r.staged.clone(),
        }
    }
}

/// Authenticate, then check the caller may publish under `publisher`.
async fn authorise(req: &Request, env: &Env, publisher: &str) -> Result<ApiResult<auth::Caller>> {
    let db = env.d1("DB")?;
    let caller = match auth::authenticate(&db, req, &time::rfc3339(now_ms())).await? {
        Ok(c) => c,
        Err(e) => return Ok(Err(e)),
    };
    if !auth::may_publish(&db, &caller.account, publisher).await? {
        return Ok(Err(ApiError::namespace_not_yours(format!(
            "Account {} may not publish under {publisher}/. Namespaces are granted by the registry maintainer.",
            caller.account
        ))));
    }
    Ok(Ok(caller))
}

async fn whoami(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let db = ctx.env.d1("DB")?;
    let outcome = async {
        let caller = match auth::authenticate(&db, &req, &time::rfc3339(now_ms())).await? {
            Ok(c) => c,
            Err(e) => return Ok(Err(e)),
        };
        let publishers = auth::namespaces(&db, &caller.account).await?;
        Ok(Ok(WhoAmI {
            account: caller.account,
            publishers,
            token_expires: caller.expires,
        }))
    };
    respond(outcome.await)
}

async fn logout(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let db = ctx.env.d1("DB")?;
    let now = time::rfc3339(now_ms());
    let outcome = async {
        let caller = match auth::authenticate(&db, &req, &now).await? {
            Ok(c) => c,
            Err(e) => return Ok(Err(e)),
        };
        auth::revoke(&db, &caller.token_hash, &now).await?;
        Ok(Ok(serde_json::json!({ "revoked": true })))
    };
    respond(outcome.await)
}

async fn reserve(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let mut buf = String::new();
    let name = match package_name(param(&ctx, "publisher"), param(&ctx, "name"), &mut buf) {
        Ok(n) => n,
        Err(e) => return e.into_response(),
    };
    let caller = match authorise(&req, &ctx.env, name.publisher()).await? {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    let manifest = req.text().await?;
    match pkg::parse(&manifest) {
        Err(e) => return ApiError::manifest_invalid(e.to_string()).into_response(),
        Ok(m) if m.manifest().name != name => {
            return ApiError::manifest_invalid(format!(
                "The manifest is for {}, but this request reserves {name}.",
                m.manifest().name
            ))
            .into_response()
        }
        Ok(_) => {}
    }
    let request = ReserveRequest {
        name: name.to_string(),
        manifest,
        account: caller.account,
        now_ms: now_ms(),
    };
    let mut resp = call(&ctx.env, &name, "/reserve", &request).await?;
    if resp.status_code() != 200 {
        return Ok(resp);
    }
    let r: Reservation = resp.json().await?;
    Response::from_json(&ReservationOut::from(&r))
}

async fn upload(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let mut buf = String::new();
    let name = match package_name(param(&ctx, "publisher"), param(&ctx, "name"), &mut buf) {
        Ok(n) => n,
        Err(e) => return e.into_response(),
    };
    let file = param(&ctx, "file").to_string();
    if !keys::is_package_file(&file) {
        return ApiError::file_not_in_manifest(format!("{file} is not a file a package can contain.")).into_response();
    }
    let caller = match authorise(&req, &ctx.env, name.publisher()).await? {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    let mut r = UploadRef {
        name: name.to_string(),
        version: param(&ctx, "version").to_string(),
        id: param(&ctx, "upload").to_string(),
        account: caller.account,
        now_ms: now_ms(),
        file: None,
    };
    let version: Version = match r.version.parse() {
        Ok(v) => v,
        Err(e) => return ApiError::not_found(format!("\"{}\" is not a version: {e}", r.version)).into_response(),
    };

    // The reservation decides what this file must be, before a byte of it is read.
    let mut resp = call(&ctx.env, &name, "/reservation", &r).await?;
    if resp.status_code() != 200 {
        return Ok(resp);
    }
    let res: Reservation = resp.json().await?;
    if res.published.is_some() {
        return ApiError::version_exists(format!("{name}@{version} is already published.")).into_response();
    }
    if !res.files().contains(&file) {
        return ApiError::file_not_in_manifest(format!("The manifest of {name}@{version} names no {file}."))
            .into_response();
    }
    let owned = pkg::parse(&res.manifest).expect("validated at reservation");
    let manifest = owned.manifest();
    // Refuse an oversized body from its declared length, without reading it.
    let limit = manifest
        .artifacts
        .iter()
        .find(|a| a.arch.artifact_file() == file)
        .map(|a| a.size)
        .unwrap_or(pkg::MAX_ARTIFACT_SIZE);
    if let Some(declared) = req.headers().get("Content-Length")?.and_then(|l| l.parse::<u64>().ok()) {
        if declared > limit {
            return ApiError::size_mismatch(format!("{file}: {declared} bytes offered, at most {limit} expected."))
                .into_response();
        }
    }
    let bytes = req.bytes().await?;
    match object::check_file(&res.manifest, &manifest, &file, &bytes) {
        Err(e) => return e.into_response(),
        Ok(Some(key)) => {
            if !auth::key_registered(&ctx.env.d1("DB")?, name.publisher(), &key.to_string()).await? {
                return object::unregistered(name.publisher(), &key).into_response();
            }
        }
        Ok(None) => {}
    }
    ctx.env
        .bucket("PACKAGES")?
        .put(keys::staged(&name, version, &res.id, &file), bytes)
        .execute()
        .await?;
    r.file = Some(file);
    let mut resp = call(&ctx.env, &name, "/staged", &r).await?;
    if resp.status_code() != 200 {
        return Ok(resp);
    }
    let res: Reservation = resp.json().await?;
    Response::from_json(&ReservationOut::from(&res))
}

async fn finalise(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let mut buf = String::new();
    let name = match package_name(param(&ctx, "publisher"), param(&ctx, "name"), &mut buf) {
        Ok(n) => n,
        Err(e) => return e.into_response(),
    };
    let caller = match authorise(&req, &ctx.env, name.publisher()).await? {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    let r = UploadRef {
        name: name.to_string(),
        version: param(&ctx, "version").to_string(),
        id: param(&ctx, "upload").to_string(),
        account: caller.account,
        now_ms: now_ms(),
        file: None,
    };
    call(&ctx.env, &name, "/finalise", &r).await
}

/// Serve a public file from the packages bucket.
async fn read(key: String, env: &Env) -> Result<Response> {
    let Some(obj) = env.bucket("PACKAGES")?.get(&key).execute().await? else {
        return ApiError::not_found(format!("{key} does not exist.")).into_response();
    };
    let meta = obj.http_metadata();
    let bytes = match obj.body() {
        Some(b) => b.bytes().await?,
        None => Vec::new(),
    };
    let headers = Headers::new();
    headers.set(
        "Content-Type",
        meta.content_type.as_deref().unwrap_or("application/octet-stream"),
    )?;
    if let Some(c) = meta.cache_control {
        headers.set("Cache-Control", &c)?;
    }
    Ok(Response::from_bytes(bytes)?.with_headers(headers))
}

const ABOUT: &str = "Godspeed package registry.\n\
    Read: GET /index/<publisher>/<name>.toml, GET /packages/<publisher>/<name>/<version>/<file>\n\
    Publish: use `godspeed publish`.\n";

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    Router::new()
        .get("/", |_, _| Response::ok(ABOUT))
        .get_async("/v1/whoami", whoami)
        .post_async("/v1/logout", logout)
        .post_async("/v1/packages/:publisher/:name/reservations", reserve)
        .put_async("/v1/packages/:publisher/:name/:version/uploads/:upload/:file", upload)
        .post_async(
            "/v1/packages/:publisher/:name/:version/uploads/:upload/finalise",
            finalise,
        )
        .get_async("/index/:publisher/:file", |_, ctx| async move {
            let key = format!("index/{}/{}", param(&ctx, "publisher"), param(&ctx, "file"));
            read(key, &ctx.env).await
        })
        .get_async("/packages/:publisher/:name/:version/:file", |_, ctx| async move {
            let key = format!(
                "packages/{}/{}/{}/{}",
                param(&ctx, "publisher"),
                param(&ctx, "name"),
                param(&ctx, "version"),
                param(&ctx, "file")
            );
            read(key, &ctx.env).await
        })
        .run(req, env)
        .await
}
