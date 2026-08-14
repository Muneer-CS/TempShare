use crate::auth::RateLimiter;
use crate::db::Pool;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

#[derive(Clone)]
pub struct UnlockToken {
    pub share_id: String,
    pub client_ip: IpAddr,
    pub expires_at: Instant,
}

#[derive(Clone)]
pub struct DownloadSession {
    pub share_id: String,
    pub client_ip: IpAddr,
    pub expires_at: Instant,
    pub counted: bool,
}

pub struct Config {
    pub bind_addr: String,
    pub public_bind_addr: String,
    pub public_base_url: String,
    pub auto_tunnel: bool,
    pub storage_dir: PathBuf,
    pub db_path: String,
    pub max_upload_bytes: u64,
    pub global_rate_limit_per_min: u32,
    pub failed_auth_max: i64,
    pub failed_auth_window_secs: i64,
    pub unlock_token_ttl_secs: u64,
    /// Maximum bytes per second for each download connection. Zero disables throttling.
    pub bandwidth_bytes_per_sec: u64,
}

impl Config {
    pub fn from_env() -> Self {
        let storage_dir = std::env::var("TEMPSHARE_STORAGE_DIR")
            .unwrap_or_else(|_| "./data/shared_files".to_string())
            .into();
        let db_path = std::env::var("TEMPSHARE_DB_PATH")
            .unwrap_or_else(|_| "./data/tempshare.db".to_string());
        let bind_addr =
            std::env::var("TEMPSHARE_BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:7420".to_string());
        let public_bind_addr = std::env::var("TEMPSHARE_PUBLIC_BIND_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:7421".to_string());
        let public_base_url = std::env::var("TEMPSHARE_PUBLIC_BASE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:7421".to_string())
            .trim_end_matches('/')
            .to_string();
        let auto_tunnel = std::env::var("TEMPSHARE_AUTO_TUNNEL")
            .map(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);
        let max_upload_bytes = std::env::var("TEMPSHARE_MAX_UPLOAD_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1024u64 * 1024 * 1024 * 50); // 50 GB default cap on a single share
        let global_rate_limit_per_min = std::env::var("TEMPSHARE_RATE_LIMIT_PER_MIN")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(120);
        Config {
            bind_addr,
            public_bind_addr,
            public_base_url,
            auto_tunnel,
            storage_dir,
            db_path,
            max_upload_bytes,
            global_rate_limit_per_min,
            failed_auth_max: 5,
            failed_auth_window_secs: 300,
            unlock_token_ttl_secs: std::env::var("TEMPSHARE_UNLOCK_TOKEN_TTL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(900),
            bandwidth_bytes_per_sec: std::env::var("TEMPSHARE_BANDWIDTH_BYTES_PER_SEC")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        }
    }
}

pub struct AppState {
    pub db: Pool,
    pub config: Config,
    pub rate_limiter: RateLimiter,
    /// Session tokens for shares that have been password-unlocked, so the
    /// browser doesn't need to resend the plaintext password on every
    /// Range-request chunk. Value = share_id it's valid for.
    pub unlock_tokens: std::sync::Mutex<std::collections::HashMap<String, UnlockToken>>,
    /// Serializes the failure-count check and failure recording so parallel
    /// guesses cannot all pass the same pre-check.
    pub failed_auth_lock: std::sync::Mutex<()>,
    pub download_sessions: std::sync::Mutex<std::collections::HashMap<String, DownloadSession>>,
    public_base_url: std::sync::RwLock<String>,
    tunnel_status: std::sync::RwLock<String>,
}

pub type SharedState = Arc<AppState>;

impl AppState {
    pub fn new(db: Pool, config: Config) -> SharedState {
        let public_base_url = config.public_base_url.clone();
        let tunnel_status = if config.auto_tunnel {
            "starting".to_string()
        } else {
            "disabled".to_string()
        };
        Arc::new(AppState {
            rate_limiter: RateLimiter::new(
                config.global_rate_limit_per_min,
                Duration::from_secs(60),
            ),
            db,
            config,
            unlock_tokens: std::sync::Mutex::new(std::collections::HashMap::new()),
            failed_auth_lock: std::sync::Mutex::new(()),
            download_sessions: std::sync::Mutex::new(std::collections::HashMap::new()),
            public_base_url: std::sync::RwLock::new(public_base_url),
            tunnel_status: std::sync::RwLock::new(tunnel_status),
        })
    }

    pub fn public_base_url(&self) -> String {
        self.public_base_url.read().unwrap().clone()
    }

    pub fn set_public_base_url(&self, url: String) {
        *self.public_base_url.write().unwrap() = url;
        *self.tunnel_status.write().unwrap() = "connected".to_string();
    }

    pub fn set_tunnel_status(&self, status: impl Into<String>) {
        *self.tunnel_status.write().unwrap() = status.into();
    }

    pub fn tunnel_status(&self) -> String {
        self.tunnel_status.read().unwrap().clone()
    }

    pub fn sweep_unlock_tokens(&self) {
        let now = Instant::now();
        self.unlock_tokens
            .lock()
            .unwrap()
            .retain(|_, token| token.expires_at > now);
        self.download_sessions
            .lock()
            .unwrap()
            .retain(|_, session| session.expires_at > now);
    }
}
