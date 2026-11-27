//! The portal web app at `/admin/`: `portal/dist` embedded at build time with the
//! `portal-ui` cargo feature (unknown paths get `index.html`, the app routes itself).
//! Without the feature, `/admin` answers a short text page.
//!
//! `/admin/magic?token=…` is a page of the app: it shows a "Sign in" button and only the
//! button's POST to `/api/v1/auth/magic` uses the link, so mail scanners that open
//! links change nothing.

use axum::Router;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;

/// Response headers for every `/admin` page.
fn page_headers(h: &mut HeaderMap) {
    // The magic-link token is in the URL: never send it on as a Referer.
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; \
             frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    );
}

pub fn routes() -> Router {
    Router::new()
        .route("/admin", get(|| async { Redirect::permanent("/admin/") }))
        .route("/admin/", get(|| serve(String::new())))
        .route(
            "/admin/{*path}",
            get(|axum::extract::Path(p): axum::extract::Path<String>| serve(p)),
        )
}

#[cfg(feature = "portal-ui")]
#[derive(rust_embed::RustEmbed)]
#[folder = "../../portal/dist"]
#[allow_missing = true]
struct Assets;

#[cfg(feature = "portal-ui")]
async fn serve(path: String) -> Response {
    let path = path.trim_start_matches('/');
    let (file, name) = match Assets::get(path).filter(|_| !path.is_empty()) {
        Some(f) => (f, path),
        // A missing file (`.js`, `.png`, …) is a 404; any other path is a page of the app.
        None if path.rsplit('/').next().is_some_and(|l| l.contains('.')) => {
            return StatusCode::NOT_FOUND.into_response();
        }
        None => match Assets::get("index.html") {
            Some(f) => (f, "index.html"),
            None => return no_ui(),
        },
    };
    let mut resp = file.data.into_owned().into_response();
    let h = resp.headers_mut();
    page_headers(h);
    if let Ok(v) = HeaderValue::from_str(file.metadata.mimetype()) {
        h.insert(header::CONTENT_TYPE, v);
    }
    // Vite puts content-hashed files under assets/; everything else may change.
    let cache = if name.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    resp
}

#[cfg(not(feature = "portal-ui"))]
async fn serve(_path: String) -> Response {
    no_ui()
}

fn no_ui() -> Response {
    let mut resp = (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "This Rconn server was built without the admin portal's web app \
         (cargo feature `portal-ui`). The portal API is at /api/v1.\n",
    )
        .into_response();
    page_headers(resp.headers_mut());
    resp
}
