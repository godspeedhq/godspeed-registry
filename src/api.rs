//! The errors the publishing API returns: a stable machine-readable `error` code the CLI maps to a
//! diagnostic, a human `message`, and the HTTP status. The codes are part of the registry protocol and
//! change only with a new API version (`/v2`).

use serde::Serialize;
use worker::Response;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ApiError {
    #[serde(skip)]
    pub status: u16,
    pub error: &'static str,
    pub message: String,
}

macro_rules! codes {
    ($($fn:ident => $status:literal, $code:literal;)*) => {
        impl ApiError {
            $(
                pub fn $fn(message: impl Into<String>) -> Self {
                    ApiError { status: $status, error: $code, message: message.into() }
                }
            )*
        }
        /// Every error code, for tests and documentation.
        #[cfg(test)]
        pub const CODES: &[&str] = &[$($code),*];
    };
}

codes! {
    unauthenticated         => 401, "unauthenticated";
    token_expired           => 401, "token-expired";
    namespace_not_yours     => 403, "namespace-not-yours";
    not_found               => 404, "not-found";
    version_exists          => 409, "version-exists";
    version_being_published => 409, "version-being-published";
    upload_incomplete       => 409, "upload-incomplete";
    reservation_expired     => 410, "reservation-expired";
    manifest_invalid        => 400, "manifest-invalid";
    file_not_in_manifest    => 400, "file-not-in-manifest";
    size_mismatch           => 422, "size-mismatch";
    digest_mismatch         => 422, "digest-mismatch";
    signature_invalid       => 422, "signature-invalid";
    key_not_registered      => 403, "key-not-registered";
    trust_not_initialised   => 503, "trust-not-initialised";
    trust_expired           => 503, "trust-expired";
    conflict                => 409, "published-differently";
    unavailable             => 503, "registry-unavailable";
}

impl ApiError {
    pub fn into_response(self) -> worker::Result<Response> {
        Ok(Response::from_json(&self)?.with_status(self.status))
    }
}

/// A handler's outcome: a JSON success, or an [`ApiError`].
pub type ApiResult<T> = std::result::Result<T, ApiError>;

/// Turn an outcome into the response the client sees. A storage or runtime failure is reported as
/// `registry-unavailable` - an outcome the client must treat as unknown, never as a refusal.
pub fn respond<T: Serialize>(outcome: worker::Result<ApiResult<T>>) -> worker::Result<Response> {
    match outcome {
        Ok(Ok(body)) => Response::from_json(&body),
        Ok(Err(e)) => e.into_response(),
        Err(e) => ApiError::unavailable(format!("the registry could not complete the request: {e}")).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique_and_kebab_case() {
        let mut seen = std::collections::HashSet::new();
        for c in CODES {
            assert!(seen.insert(*c), "{c} twice");
            assert!(c.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'), "{c}");
        }
    }
}
