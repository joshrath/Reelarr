//! A small JSON client for the Reelarr API, used from the browser.
//!
//! Requests are same-origin and send the session cookie. Reads need no
//! CSRF header; changes (`POST`, `PUT`, `DELETE`) send the readable
//! `reelarr_csrf` cookie's value as `X-CSRF-Token`, the double-submit proof
//! the server checks.

use reelarr_types::ApiError;
use serde::{Serialize, de::DeserializeOwned};

/// The readable cookie holding the session's CSRF token.
pub const CSRF_COOKIE: &str = "reelarr_csrf";
/// The header a change sends the CSRF token in.
pub const CSRF_HEADER: &str = "X-CSRF-Token";

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

/// Why a change failed. Keeps the server's message for the few rejections
/// a page may repeat (validation and conflicts, whose messages are static
/// or quote only what was typed); everything else gets the page's own words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendFailure {
    pub failure: ApiFailure,
    pub message: Option<String>,
}

impl SendFailure {
    pub fn from_response(status: u16, body: &str) -> Self {
        let error = serde_json::from_str::<ApiError>(body).ok();
        Self {
            failure: ApiFailure::Status {
                status,
                code: error.as_ref().map(|e| e.code.clone()),
            },
            message: error.map(|e| e.message),
        }
    }

    pub fn network() -> Self {
        Self {
            failure: ApiFailure::Network,
            message: None,
        }
    }

    /// The JSON error body's `code`, if the server sent one.
    pub fn code(&self) -> Option<&str> {
        match &self.failure {
            ApiFailure::Status { code, .. } => code.as_deref(),
            ApiFailure::Network => None,
        }
    }

    pub fn status(&self) -> Option<u16> {
        match &self.failure {
            ApiFailure::Status { status, .. } => Some(*status),
            ApiFailure::Network => None,
        }
    }
}

/// A change's HTTP method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Post,
    Put,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
        }
    }
}

/// The value of cookie `name` in a `document.cookie` string.
pub fn cookie_value(cookies: &str, name: &str) -> Option<String> {
    cookies
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then(|| value.to_string()))
}

/// Send a change with an optional JSON body and read a JSON answer.
///
/// Only does anything in the browser (`hydrate`); the server never runs it.
pub async fn send_json<B: Serialize, T: DeserializeOwned>(
    method: Method,
    path: &str,
    body: Option<&B>,
) -> Result<T, SendFailure> {
    let text = send(method, path, body).await?;
    serde_json::from_str(&text).map_err(|_| SendFailure::network())
}

/// Send a change whose answer has no body we need (e.g. `204 No Content`).
pub async fn send_empty<B: Serialize>(
    method: Method,
    path: &str,
    body: Option<&B>,
) -> Result<(), SendFailure> {
    send(method, path, body).await.map(|_| ())
}

async fn send<B: Serialize>(
    method: Method,
    path: &str,
    body: Option<&B>,
) -> Result<String, SendFailure> {
    let json = match body {
        Some(body) => Some(serde_json::to_string(body).map_err(|_| SendFailure::network())?),
        None => None,
    };
    #[cfg(feature = "hydrate")]
    {
        browser::send(method, path, json).await
    }
    #[cfg(not(feature = "hydrate"))]
    {
        let _ = (method, path, json);
        Err(SendFailure::network())
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
    use super::{ApiFailure, CSRF_COOKIE, CSRF_HEADER, Method, SendFailure, cookie_value};
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

    /// The CSRF token from the readable cookie, if the session set one.
    fn csrf_token() -> Option<String> {
        let document = web_sys::window()?.document()?;
        let cookies = document
            .dyn_into::<web_sys::HtmlDocument>()
            .ok()?
            .cookie()
            .ok()?;
        cookie_value(&cookies, CSRF_COOKIE)
    }

    pub async fn send(
        method: Method,
        path: &str,
        json: Option<String>,
    ) -> Result<String, SendFailure> {
        let network = |_| SendFailure::network();
        let headers = web_sys::Headers::new().map_err(network)?;
        if let Some(token) = csrf_token() {
            headers.set(CSRF_HEADER, &token).map_err(network)?;
        }
        let init = web_sys::RequestInit::new();
        init.set_method(method.as_str());
        init.set_credentials(web_sys::RequestCredentials::SameOrigin);
        if let Some(json) = json {
            headers
                .set("Content-Type", "application/json")
                .map_err(network)?;
            init.set_body(&json.into());
        }
        init.set_headers(&headers);
        let window = web_sys::window().ok_or_else(SendFailure::network)?;
        let response: web_sys::Response =
            JsFuture::from(window.fetch_with_str_and_init(path, &init))
                .await
                .map_err(network)?
                .dyn_into()
                .map_err(network)?;
        let text = JsFuture::from(response.text().map_err(network)?)
            .await
            .map_err(network)?
            .as_string()
            .unwrap_or_default();
        if !response.ok() {
            return Err(SendFailure::from_response(response.status(), &text));
        }
        Ok(text)
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
    fn send_failure_keeps_code_and_message() {
        let failure = SendFailure::from_response(
            400,
            r#"{"code":"invalid_settings","message":"invalid NNTP settings: retries must be 0–10"}"#,
        );
        assert_eq!(failure.code(), Some("invalid_settings"));
        assert_eq!(failure.status(), Some(400));
        assert_eq!(
            failure.message.as_deref(),
            Some("invalid NNTP settings: retries must be 0–10")
        );
        let plain = SendFailure::from_response(502, "Bad Gateway");
        assert_eq!(plain.code(), None);
        assert_eq!(plain.message, None);
        assert_eq!(SendFailure::network().status(), None);
    }

    #[test]
    fn csrf_cookie_is_found_among_others() {
        let cookies = "reelarr_theme=80s-vhs; reelarr_csrf=abc123; reelarr_crt=on";
        assert_eq!(
            cookie_value(cookies, CSRF_COOKIE).as_deref(),
            Some("abc123")
        );
        assert_eq!(cookie_value("reelarr_csrf_old=x", CSRF_COOKIE), None);
        assert_eq!(cookie_value("", CSRF_COOKIE), None);
    }

    #[test]
    fn methods_are_upper_case() {
        assert_eq!(Method::Post.as_str(), "POST");
        assert_eq!(Method::Put.as_str(), "PUT");
        assert_eq!(Method::Delete.as_str(), "DELETE");
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
