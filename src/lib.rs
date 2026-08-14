pub mod auth;
pub mod db;
pub mod download;
pub mod error;
pub mod ids;
pub mod shares;
pub mod state;

use axum::routing::{get, patch, post};
use axum::{
    body::Body,
    extract::DefaultBodyLimit,
    http::{header, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Router,
};
use state::SharedState;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

/// Routes reachable ONLY from the local machine (management UI + API).
/// These can create/revoke/delete shares and must never be reachable from
/// a tunnel or port-forward -- only `public_router` should ever be exposed
/// to the internet.
pub fn management_router(state: SharedState) -> Router {
    let upload_limit = usize::try_from(state.config.max_upload_bytes)
        .unwrap_or(usize::MAX)
        .saturating_add(16 * 1024 * 1024);
    let api = Router::new()
        .route(
            "/shares",
            get(shares::list_shares).post(shares::create_share),
        )
        .route("/shares/folder", post(shares::create_folder_share))
        .route("/status", get(shares::server_status))
        .route(
            "/shares/:id",
            patch(shares::update_share).delete(shares::delete_share),
        )
        .route("/shares/:id/revoke", post(shares::revoke_share))
        .route("/download/:id/info", get(download::share_info))
        .route("/download/:id/unlock", post(download::unlock_share))
        .layer(DefaultBodyLimit::max(upload_limit))
        .with_state(state.clone());

    let static_files =
        ServeDir::new("static").not_found_service(ServeFile::new("static/index.html"));

    Router::new()
        .nest("/api", api)
        .route("/download/:id", get(download::download))
        .route("/s/:id", get(download::share_page))
        .with_state(state)
        .fallback_service(static_files)
        .layer(middleware::from_fn(validate_management_request))
        .layer(TraceLayer::new_for_http())
}

async fn validate_management_request(request: Request<Body>, next: Next) -> Response {
    if let Some(host) = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    {
        let hostname = host
            .strip_prefix('[')
            .and_then(|value| value.split_once(']').map(|(name, _)| name))
            .unwrap_or_else(|| host.split(':').next().unwrap_or(host));
        if !matches!(
            hostname.to_ascii_lowercase().as_str(),
            "127.0.0.1" | "localhost" | "::1"
        ) {
            return (StatusCode::FORBIDDEN, "management interface is local-only").into_response();
        }
    }
    if let Some(origin) = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    {
        let allowed = [
            "http://127.0.0.1:",
            "http://localhost:",
            "https://127.0.0.1:",
            "https://localhost:",
        ]
        .iter()
        .any(|prefix| origin.to_ascii_lowercase().starts_with(prefix));
        if !allowed {
            return (
                StatusCode::FORBIDDEN,
                "cross-origin management request rejected",
            )
                .into_response();
        }
    }
    next.run(request).await
}

/// Routes safe to expose to the internet (via Cloudflare Tunnel, Tailscale
/// Funnel, reverse proxy, or port forward). Deliberately minimal: it can
/// only ever resolve an opaque share ID to a stream of bytes.
pub fn public_router(state: SharedState) -> Router {
    Router::new()
        .route("/s/:id", get(download::share_page))
        .route("/download/:id", get(download::download))
        .route("/api/download/:id/info", get(download::share_info))
        .route("/api/download/:id/unlock", post(download::unlock_share))
        .with_state(state)
        .layer(TraceLayer::new_for_http())
}
