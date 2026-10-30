//! Bearer-token protection for the admin endpoints.

use axum::{
    extract::{Request, State},
    http::header::AUTHORIZATION,
    middleware::Next,
    response::Response,
};
use subtle::ConstantTimeEq;

use crate::{AppState, error::AppError};

/// Lets the request through if no admin token is configured, or if it carries
/// `Authorization: Bearer <token>` with the right token.
pub async fn require_admin(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    if let Some(expected) = state.config.admin_token() {
        let provided = request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(bearer_token);
        if !provided.is_some_and(|token| tokens_match(token.as_bytes(), expected.expose())) {
            tracing::warn!(path = %request.uri().path(), "rejected admin request without a valid token");
            return Err(AppError::AdminUnauthorized);
        }
    }
    Ok(next.run(request).await)
}

/// The token from an `Authorization` header value. The scheme is case-insensitive.
fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

/// Constant-time in the token's contents, so a guess can't be refined byte by byte.
fn tokens_match(provided: &[u8], expected: &[u8]) -> bool {
    provided.ct_eq(expected).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bearer_tokens() {
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("bearer  abc "), Some("abc"));
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bearer"), None);
    }

    #[test]
    fn compares_tokens() {
        assert!(tokens_match(b"s3cret", b"s3cret"));
        assert!(!tokens_match(b"s3cret", b"s3cres"));
        assert!(!tokens_match(b"s3cre", b"s3cret"));
        assert!(!tokens_match(b"", b"s3cret"));
    }
}
