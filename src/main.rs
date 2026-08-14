use std::net::SocketAddr;
use tempshare::state::{AppState, Config};
use tempshare::{db, management_router, public_router};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv(); // ok if .env doesn't exist; real env vars still work

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tempshare=info,tower_http=info".into()),
        )
        .init();

    let config = Config::from_env();
    std::fs::create_dir_all(&config.storage_dir)?;
    if let Some(parent) = std::path::Path::new(&config.db_path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    let pool = db::init_pool(&config.db_path)?;
    let state = AppState::new(pool, config);
    prevent_system_sleep();

    // Background cleanup: revoke expired shares and sweep old rate-limit
    // entries every 60 seconds. This never deletes files -- see the doc
    // comment on db::delete_expired_shares.
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                let now = chrono::Utc::now().timestamp();
                match db::delete_expired_shares(&state.db, now) {
                    Ok(expired) if !expired.is_empty() => {
                        tracing::info!("expired {} share(s)", expired.len());
                    }
                    Err(e) => tracing::error!("cleanup error: {e:#}"),
                    _ => {}
                }
                state.rate_limiter.sweep();
                state.sweep_unlock_tokens();
            }
        });
    }

    let mgmt_addr: SocketAddr = state.config.bind_addr.parse()?;
    let public_addr: SocketAddr = state.config.public_bind_addr.parse()?;

    let mgmt_app = management_router(state.clone());
    let public_app = public_router(state.clone());

    tracing::info!("management UI + API listening on http://{mgmt_addr}");
    tracing::info!(
        "public download listener on {public_addr} -- point Cloudflare Tunnel / Tailscale / your reverse proxy here, NOT at the management address"
    );

    let mgmt_listener = tokio::net::TcpListener::bind(mgmt_addr).await?;
    let public_listener = tokio::net::TcpListener::bind(public_addr).await?;

    if state.config.auto_tunnel {
        start_quick_tunnel(state.clone(), public_addr.port());
    }

    let mgmt_server = axum::serve(
        mgmt_listener,
        mgmt_app.into_make_service_with_connect_info::<SocketAddr>(),
    );
    let public_server = axum::serve(
        public_listener,
        public_app.into_make_service_with_connect_info::<SocketAddr>(),
    );

    tokio::try_join!(
        async { mgmt_server.await.map_err(anyhow::Error::from) },
        async { public_server.await.map_err(anyhow::Error::from) },
    )?;

    Ok(())
}

fn start_quick_tunnel(state: tempshare::state::SharedState, public_port: u16) {
    tokio::spawn(async move {
        let executable_dir = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(std::path::Path::to_path_buf))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let cloudflared = executable_dir.join("cloudflared.exe");
        let config = executable_dir.join("cloudflared-quick.yml");
        if !cloudflared.is_file() {
            state.set_tunnel_status("cloudflared_missing");
            tracing::error!("automatic tunnel enabled but cloudflared.exe is missing");
            return;
        }

        let mut retry_delay = 2u64;
        loop {
            state.set_tunnel_status("starting");
            let mut command = Command::new(&cloudflared);
            command
                .arg("--config")
                .arg(&config)
                .arg("tunnel")
                .arg("--no-autoupdate")
                .arg("--url")
                .arg(format!("http://127.0.0.1:{public_port}"))
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) => {
                    state.set_tunnel_status("start_failed");
                    tracing::error!("could not start cloudflared: {error}");
                    tokio::time::sleep(std::time::Duration::from_secs(retry_delay)).await;
                    retry_delay = (retry_delay * 2).min(30);
                    continue;
                }
            };
            let Some(stderr) = child.stderr.take() else {
                state.set_tunnel_status("start_failed");
                return;
            };
            let mut connected = false;
            let mut registered = false;
            let mut pending_url = None;
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(url) = trycloudflare_url(&line) {
                    pending_url = Some(url);
                }
                if line.contains("Registered tunnel connection") {
                    registered = true;
                }
                if registered {
                    if let Some(url) = pending_url.take() {
                        // Give the new hostname a moment to propagate before
                        // telling the dashboard it is ready to share.
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        tracing::info!("public HTTPS share URL: {url}");
                        state.set_public_base_url(url);
                        connected = true;
                        retry_delay = 2;
                    }
                }
            }
            match child.wait().await {
                Ok(status) => tracing::warn!("cloudflared stopped with {status}; reconnecting"),
                Err(error) => tracing::error!("cloudflared wait failed: {error}"),
            }
            state.set_tunnel_status(if connected { "reconnecting" } else { "failed" });
            tokio::time::sleep(std::time::Duration::from_secs(retry_delay)).await;
            retry_delay = (retry_delay * 2).min(30);
        }
    });
}

#[cfg(windows)]
fn prevent_system_sleep() {
    const ES_CONTINUOUS: u32 = 0x8000_0000;
    const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetThreadExecutionState(flags: u32) -> u32;
    }
    let result = unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
    if result == 0 {
        tracing::warn!("Windows sleep prevention could not be enabled");
    } else {
        tracing::info!("Windows sleep prevention active while TempShare is running");
    }
}

#[cfg(not(windows))]
fn prevent_system_sleep() {}

fn trycloudflare_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let value: String = line[start..]
        .chars()
        .take_while(|character| {
            character.is_ascii_alphanumeric() || matches!(character, ':' | '/' | '.' | '-')
        })
        .collect();
    if value.ends_with(".trycloudflare.com") {
        Some(value)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::trycloudflare_url;

    #[test]
    fn extracts_quick_tunnel_url_from_log_line() {
        assert_eq!(
            trycloudflare_url(
                "INF Your quick Tunnel has been created! url=https://quiet-tree.trycloudflare.com"
            )
            .as_deref(),
            Some("https://quiet-tree.trycloudflare.com")
        );
        assert_eq!(trycloudflare_url("https://example.com"), None);
    }
}
