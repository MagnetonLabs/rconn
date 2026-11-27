//! Request checks for the whole API, and cookie helpers.
//!
//! CSRF: the session cookie is SameSite=Lax, every non-GET request must carry an `Origin`
//! equal to the portal's, and bodies must be JSON (a cross-site form can't send that
//! without a CORS preflight, which this server never grants).

use super::{ApiError, PortalState};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Every API request passes here.
pub async fn guard(State(st): State<PortalState>, req: Request, next: Next) -> Response {
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        if !origin_ok(&st, req.headers()) {
            return ApiError::Forbidden("request from another site refused").into_response();
        }
        // POST, PUT and PATCH always carry a JSON body (`{}` when the endpoint takes
        // none); DELETE has no body, but a content type it sends must still be JSON.
        let needs_body = matches!(*req.method(), Method::POST | Method::PUT | Method::PATCH);
        match req.headers().get(header::CONTENT_TYPE) {
            Some(ct) if !is_json(ct) => return ApiError::NotJson.into_response(),
            None if needs_body => return ApiError::NotJson.into_response(),
            _ => {}
        }
    }
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    resp
}

fn is_json(ct: &HeaderValue) -> bool {
    ct.to_str()
        .ok()
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
}

/// The request's `Origin` is the portal's: `portal_url` when configured, else the
/// request's own `Host` (same-origin).
fn origin_ok(st: &PortalState, headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    match st.config.origin() {
        Some(expected) => origin.eq_ignore_ascii_case(expected),
        None => {
            let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
                return false;
            };
            origin
                .split_once("://")
                .is_some_and(|(_, authority)| authority.eq_ignore_ascii_case(host))
        }
    }
}

/// A cookie's value from the request, if present.
pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// `Set-Cookie` for a `__Host-` cookie: Secure, HttpOnly, SameSite=Lax, Path=/.
pub fn set_cookie(name: &str, value: &str, max_age_secs: u64) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{name}={value}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}"
    ))
    .expect("cookie values are base64url")
}

/// `Set-Cookie` that deletes a cookie.
pub fn clear_cookie(name: &str) -> HeaderValue {
    set_cookie(name, "", 0)
}
