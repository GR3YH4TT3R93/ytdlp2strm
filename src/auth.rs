use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

use crate::state::AppState;

fn check_basic_auth(headers: &HeaderMap, username: &str, password: &str) -> bool {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return false;
    };
    let Some(value) = value.to_str().ok() else {
        return false;
    };
    let Some(b64) = value.strip_prefix("Basic ") else {
        return false;
    };
    let Ok(decoded) = BASE64_STANDARD.decode(b64.trim()) else {
        return false;
    };
    let Ok(creds) = String::from_utf8(decoded) else {
        return false;
    };
    let Some((u, p)) = creds.split_once(':') else {
        return false;
    };
    u == username && p == password
}

pub async fn basic_auth_mw(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let cfg = state.config(); // <--- ADDED
    if !cfg.auth.is_enabled() {
        return next.run(req).await;
    }
    if check_basic_auth(req.headers(), &cfg.auth.username, &cfg.auth.password) {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"ytdlp2strm\"")],
            "unauthorized",
        )
            .into_response()
    }
}
