//! A small JSON client for the Reelarr API, used from the browser.
//!
//! Requests are same-origin and send the session cookie. Only reads live
//! here so far, which need no CSRF header.

use reelarr_types::ApiError;
use serde::de::DeserializeOwned;

/// Why an API request failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiFailure {
    /// The server answered with an error status. `code` is the `code` field
    /// of the JSON error body, when there was one.
    Status { status: u16, code: Option<String> },
    /// No usable answer: the network failed or the body wasn't the JSON we
    /// expected.
    Network,
}

impl ApiFailure {
    /// Builds a failure from an error response's status and body.
    pub fn from_response(status: u16, body: &str) -> Self {
        let code = serde_json::from_str::<ApiError>(body).ok().map(|e| e.code);
        Self::Status { status, code }
    }
}

/// `GET` a JSON resource from the API, e.g. `/api/v1/media/1/playback`.
///
/// Only does anything in the browser (`hydrate`); the server never runs it.
pub async fn get_json<T: DeserializeOwned>(path: &str) -> Result<T, ApiFailure> {
    #[cfg(feature = "hydrate")]
    {
        browser::get_json(path).await
    }
    #[cfg(not(feature = "hydrate"))]
    {
        let _ = path;
        Err(ApiFailure::Network)
    }
}

#[cfg(feature = "hydrate")]
mod browser {
    use super::ApiFailure;
    use serde::de::DeserializeOwned;
    use wasm_bindgen_futures::JsFuture;
    use web_sys::wasm_bindgen::JsCast;

    pub async fn get_json<T: DeserializeOwned>(path: &str) -> Result<T, ApiFailure> {
        let init = web_sys::RequestInit::new();
        init.set_method("GET");
        init.set_credentials(web_sys::RequestCredentials::SameOrigin);
        let window = web_sys::window().ok_or(ApiFailure::Network)?;
        let response: web_sys::Response =
            JsFuture::from(window.fetch_with_str_and_init(path, &init))
                .await
                .map_err(|_| ApiFailure::Network)?
                .dyn_into()
                .map_err(|_| ApiFailure::Network)?;
        let text = JsFuture::from(response.text().map_err(|_| ApiFailure::Network)?)
            .await
            .map_err(|_| ApiFailure::Network)?
            .as_string()
            .unwrap_or_default();
        if !response.ok() {
            return Err(ApiFailure::from_response(response.status(), &text));
        }
        serde_json::from_str(&text).map_err(|_| ApiFailure::Network)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_body_code_is_read() {
        assert_eq!(
            ApiFailure::from_response(503, r#"{"code":"unavailable","message":"offline"}"#),
            ApiFailure::Status {
                status: 503,
                code: Some("unavailable".into())
            }
        );
    }

    #[test]
    fn non_json_error_body_keeps_the_status() {
        assert_eq!(
            ApiFailure::from_response(502, "Bad Gateway"),
            ApiFailure::Status {
                status: 502,
                code: None
            }
        );
    }
}
