//! API errors: an HTTP status and `{"error": {"code": "...", "message": "..."}}`.

use axum::extract::{FromRequest, Request};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;

pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug)]
pub enum ApiError {
    /// 404: also the answer for anything in an org the caller isn't in.
    NotFound,
    /// 401: not signed in, or wrong credentials.
    Unauthorized,
    /// 403: signed in and a member, but the role doesn't allow it (or a server setting).
    Forbidden(&'static str),
    /// 400 `invalid`.
    Invalid(String),
    /// 400 `needs_code`: a magic link opened in another browser needs the email's code.
    NeedsCode,
    /// 409.
    Conflict(String),
    /// 415: a body that isn't JSON.
    NotJson,
    /// 429.
    RateLimited,
    /// 501: an endpoint of the contract that this server doesn't have yet.
    NotImplemented,
    /// 500; the cause is logged, never sent.
    Internal(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self::Internal(e.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found", "not found".to_owned()),
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "sign in first".to_owned(),
            ),
            Self::Forbidden(m) => (StatusCode::FORBIDDEN, "forbidden", m.to_owned()),
            Self::Invalid(m) => (StatusCode::BAD_REQUEST, "invalid", m),
            Self::NeedsCode => (
                StatusCode::BAD_REQUEST,
                "needs_code",
                "this link was opened in another browser: enter the code from the email".to_owned(),
            ),
            Self::Conflict(m) => (StatusCode::CONFLICT, "conflict", m),
            Self::NotJson => (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "invalid",
                "send the body as application/json".to_owned(),
            ),
            Self::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "too many attempts; try again later".to_owned(),
            ),
            Self::NotImplemented => (
                StatusCode::NOT_IMPLEMENTED,
                "not_implemented",
                "not available on this server yet".to_owned(),
            ),
            Self::Internal(e) => {
                tracing::error!(error = %format!("{e:#}"), "portal request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "something went wrong on the server".to_owned(),
                )
            }
        };
        (
            status,
            axum::Json(serde_json::json!({ "error": { "code": code, "message": message } })),
        )
            .into_response()
    }
}

/// A JSON body, with rejections answered in the API's error format. Use it instead of
/// `axum::Json` in portal handlers.
pub struct ApiJson<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for ApiJson<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        use axum::extract::rejection::JsonRejection;
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(v)) => Ok(Self(v)),
            Err(JsonRejection::MissingJsonContentType(_)) => Err(ApiError::NotJson),
            Err(e) => Err(ApiError::Invalid(e.body_text())),
        }
    }
}
