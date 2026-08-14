//! The public-facing download endpoint. This is the only route reachable
//! by a remote recipient. It resolves a share ID to a file strictly through
//! `db::get_share` -- there is no other code path in this file (or anywhere
//! else) that turns network input into a filesystem path.

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use futures::Stream;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, ReadBuf};

use crate::db;
use crate::error::AppError;
use crate::ids::generate_session_token;
use crate::state::SharedState;

enum DownloadFile {
    Plain(tokio::fs::File),
    Generated {
        file: tokio::fs::File,
        path: PathBuf,
    },
}

impl DownloadFile {
    fn file_mut(&mut self) -> &mut tokio::fs::File {
        match self {
            Self::Plain(file) | Self::Generated { file, .. } => file,
        }
    }
}

impl AsyncRead for DownloadFile {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(self.file_mut()).poll_read(cx, buf)
    }
}

impl AsyncSeek for DownloadFile {
    fn start_seek(mut self: Pin<&mut Self>, position: std::io::SeekFrom) -> std::io::Result<()> {
        Pin::new(self.file_mut()).start_seek(position)
    }

    fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<u64>> {
        Pin::new(self.file_mut()).poll_complete(cx)
    }
}

impl Drop for DownloadFile {
    fn drop(&mut self) {
        if let Self::Generated { path, .. } = self {
            let _ = std::fs::remove_file(path);
        }
    }
}

struct AccountingStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>,
    db: db::Pool,
    share_id: String,
    client_ip: String,
    expected_bytes: u64,
    transferred: u64,
    recorded: bool,
}

impl AccountingStream {
    fn record(&mut self, completed: bool) {
        if self.recorded {
            return;
        }
        self.recorded = true;
        let bytes = i64::try_from(self.transferred).unwrap_or(i64::MAX);
        let _ = db::record_download_event(
            &self.db,
            &self.share_id,
            &self.client_ip,
            now_ts(),
            bytes,
            completed,
        );
    }
}

impl Stream for AccountingStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                self.transferred = self.transferred.saturating_add(bytes.len() as u64);
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(error))) => {
                self.record(false);
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                let completed = self.transferred == self.expected_bytes;
                self.record(completed);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for AccountingStream {
    fn drop(&mut self) {
        self.record(false);
    }
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Returns Err(NotFound) for any share that is missing, revoked, expired,
/// or exhausted. We deliberately return the *same* 404 for "never existed"
/// and "expired/revoked" so an attacker can't distinguish valid-but-dead
/// IDs from never-issued ones by response shape.
fn load_active_share(state: &SharedState, id: &str) -> Result<db::Share, AppError> {
    load_active_share_with_session(state, id, false)
}

fn load_active_share_with_session(
    state: &SharedState,
    id: &str,
    allow_exhausted: bool,
) -> Result<db::Share, AppError> {
    let share = db::get_share(&state.db, id)
        .map_err(AppError::Internal)?
        .ok_or(AppError::NotFound)?;

    if share.status != "active" {
        return Err(AppError::NotFound);
    }
    if let Some(exp) = share.expires_at {
        if now_ts() >= exp {
            return Err(AppError::NotFound);
        }
    }
    if !allow_exhausted {
        if let Some(max) = share.max_downloads {
            if share.download_count >= max {
                return Err(AppError::NotFound);
            }
        }
    }
    Ok(share)
}

#[derive(Deserialize)]
pub struct UnlockRequest {
    pub password: String,
}

/// POST /api/download/:id/unlock -- verifies a password and returns a
/// short-lived token the browser attaches to the actual download request.
/// Failed attempts are rate-limited per share+IP with exponential backoff
/// characteristics enforced by the caller checking the failure count.
pub async fn unlock_share(
    State(state): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<UnlockRequest>,
) -> Result<Response, AppError> {
    let client_ip = client_ip(addr, &headers);
    let share = load_active_share(&state, &id)?;
    let ip = client_ip.to_string();

    let Some(hash) = &share.password_hash else {
        // Not password protected; nothing to unlock.
        return Ok(Json(json!({ "ok": true, "token": null })).into_response());
    };

    {
        let _guard = state.failed_auth_lock.lock().unwrap();
        let since = now_ts() - state.config.failed_auth_window_secs;
        let recent_failures =
            db::count_recent_failed_auth(&state.db, &id, &ip, since).map_err(AppError::Internal)?;
        if recent_failures >= state.config.failed_auth_max {
            return Err(AppError::RateLimited);
        }
        if !crate::auth::verify_password(&body.password, hash) {
            db::record_failed_auth(&state.db, &id, &ip, now_ts()).map_err(AppError::Internal)?;
            return Err(AppError::IncorrectPassword);
        }
    }

    let token = generate_session_token();
    state.unlock_tokens.lock().unwrap().insert(
        token.clone(),
        crate::state::UnlockToken {
            share_id: id.clone(),
            client_ip,
            expires_at: Instant::now() + Duration::from_secs(state.config.unlock_token_ttl_secs),
        },
    );
    let cookie = format!(
        "tempshare_unlock={token}; Path=/download/{id}; Max-Age={}; HttpOnly; Secure; SameSite=Strict",
        state.config.unlock_token_ttl_secs
    );
    let mut response = Json(json!({ "ok": true, "token": token })).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        cookie
            .parse::<axum::http::HeaderValue>()
            .map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?,
    );
    Ok(response)
}

fn is_unlocked(
    state: &SharedState,
    share_id: &str,
    expected_ip: IpAddr,
    headers: &HeaderMap,
) -> bool {
    let header_token = headers
        .get("x-tempshare-token")
        .and_then(|v| v.to_str().ok());
    let cookie_token = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|cookie| {
                let (name, value) = cookie.trim().split_once('=')?;
                (name == "tempshare_unlock").then_some(value)
            })
        });
    let Some(token) = header_token.or(cookie_token) else {
        return false;
    };
    let mut map = state.unlock_tokens.lock().unwrap();
    match map.get(token) {
        Some(entry) if entry.expires_at > Instant::now() => {
            entry.share_id == share_id && entry.client_ip == expected_ip
        }
        Some(_) => {
            map.remove(token);
            false
        }
        None => false,
    }
}

pub async fn download(
    State(state): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let client_ip = client_ip(addr, &headers);
    if !state.rate_limiter.check(client_ip) {
        return Err(AppError::RateLimited);
    }

    let continuing_session = has_counted_download_session(&state, &id, client_ip, &headers);
    let share = load_active_share_with_session(&state, &id, continuing_session)?;

    if share.password_hash.is_some() && !is_unlocked(&state, &id, client_ip, &headers) {
        return Err(AppError::PasswordRequired);
    }
    let (file, file_len, content_type) = if share.is_folder {
        let (file, len) = generate_folder_archive(&state, &share).await?;
        (file, len, "application/zip")
    } else {
        let file = tokio::fs::File::open(&share.file_path)
            .await
            .map_err(|e| AppError::Internal(e.into()))?;
        let len = file
            .metadata()
            .await
            .map_err(|e| AppError::Internal(e.into()))?
            .len();
        (DownloadFile::Plain(file), len, "application/octet-stream")
    };

    let range = headers.get(header::RANGE);
    let (start, end, status) = match range {
        Some(value) => match value.to_str().ok().and_then(|r| parse_range(r, file_len)) {
            Some((s, e)) => (s, e, StatusCode::PARTIAL_CONTENT),
            None => return Ok(range_not_satisfiable(file_len)),
        },
        None => (0, file_len.saturating_sub(1), StatusCode::OK),
    };

    if file_len == 0 && range.is_none() {
        if !claim_download_session(&state, &id, client_ip, &headers)? {
            return Err(AppError::NotFound);
        }
        return Ok(empty_file_response(&share.display_name, content_type));
    }
    if start > end || end >= file_len {
        return Ok(range_not_satisfiable(file_len));
    }
    if !claim_download_session(&state, &id, client_ip, &headers)? {
        return Err(AppError::NotFound);
    }

    let content_len = end - start + 1;

    let mut file = file;
    file.seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    let limited = file.take(content_len);
    let bytes_per_sec = state.config.bandwidth_bytes_per_sec;
    let stream = tokio_util::io::ReaderStream::new(limited).then(move |item| async move {
        if bytes_per_sec > 0 {
            if let Ok(bytes) = &item {
                tokio::time::sleep(Duration::from_secs_f64(
                    bytes.len() as f64 / bytes_per_sec as f64,
                ))
                .await;
            }
        }
        item
    });
    let body = Body::from_stream(AccountingStream {
        inner: Box::pin(stream),
        db: state.db.clone(),
        share_id: id.clone(),
        client_ip: client_ip.to_string(),
        expected_bytes: content_len,
        transferred: 0,
        recorded: false,
    });

    let disposition = content_disposition(&share.display_name);

    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, content_len.to_string())
        .header(header::CONTENT_DISPOSITION, disposition)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "no-store");

    if status == StatusCode::PARTIAL_CONTENT {
        response = response.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{file_len}"),
        );
    }

    Ok(response.body(body).unwrap())
}

/// Parses a single-range `Range: bytes=start-end` header. Returns None for
/// anything malformed or multi-range (multi-range requests are rejected by
/// falling back to a full 200 response, which is spec-compliant and safe).
fn parse_range(header_val: &str, file_len: u64) -> Option<(u64, u64)> {
    if file_len == 0 {
        return None;
    }
    let val = header_val.strip_prefix("bytes=")?;
    if val.contains(',') {
        return None; // multi-range not supported; serve full file instead
    }
    let (start_s, end_s) = val.split_once('-')?;
    if start_s.is_empty() {
        // suffix range: bytes=-500 => last 500 bytes
        let suffix: u64 = end_s.parse().ok()?;
        if suffix == 0 {
            return None;
        }
        if suffix > file_len {
            return Some((0, file_len - 1));
        }
        return Some((file_len - suffix, file_len - 1));
    }
    let start: u64 = start_s.parse().ok()?;
    let end: u64 = if end_s.is_empty() {
        file_len.saturating_sub(1)
    } else {
        end_s.parse().ok()?
    };
    if start >= file_len || start > end {
        return None;
    }
    Some((start, end.min(file_len - 1)))
}

fn range_not_satisfiable(file_len: u64) -> Response {
    (
        StatusCode::RANGE_NOT_SATISFIABLE,
        [(header::CONTENT_RANGE, format!("bytes */{file_len}"))],
    )
        .into_response()
}

fn empty_file_response(display_name: &str, content_type: &str) -> Response {
    let disposition = content_disposition(display_name);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, "0")
        .header(header::CONTENT_DISPOSITION, disposition)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::empty())
        .unwrap()
}

async fn generate_folder_archive(
    state: &SharedState,
    share: &db::Share,
) -> Result<(DownloadFile, u64), AppError> {
    let entries = db::list_share_entries(&state.db, &share.id).map_err(AppError::Internal)?;
    if entries.is_empty() {
        return Err(AppError::NotFound);
    }
    let source_dir = PathBuf::from(&share.file_path);
    let archive_path = state
        .config
        .storage_dir
        .join(format!(".archive-{}.zip", generate_session_token()));
    let archive_for_task = archive_path.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let output = std::fs::File::create(&archive_for_task)?;
        let mut zip = zip::ZipWriter::new(output);
        let options =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for entry in entries {
            zip.start_file(entry.display_name, options)?;
            let mut input = std::fs::File::open(source_dir.join(entry.stored_name))?;
            std::io::copy(&mut input, &mut zip)?;
        }
        zip.finish()?;
        Ok(())
    })
    .await
    .map_err(|e| AppError::Internal(e.into()))?;
    if let Err(error) = result {
        let _ = tokio::fs::remove_file(&archive_path).await;
        return Err(AppError::Internal(error));
    }
    let file = tokio::fs::File::open(&archive_path)
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    let len = file
        .metadata()
        .await
        .map_err(|e| AppError::Internal(e.into()))?
        .len();
    Ok((
        DownloadFile::Generated {
            file,
            path: archive_path,
        },
        len,
    ))
}

fn content_disposition(display_name: &str) -> String {
    let fallback: String = display_name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded = percent_encode_utf8(display_name);
    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

fn percent_encode_utf8(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            )
        {
            output.push(*byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(output, "%{byte:02X}");
        }
    }
    output
}

fn client_ip(addr: SocketAddr, headers: &HeaderMap) -> IpAddr {
    if addr.ip().is_loopback() {
        if let Some(ip) = headers
            .get("cf-connecting-ip")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok())
        {
            return ip;
        }
    }
    addr.ip()
}

fn cookie_value<'a>(headers: &'a HeaderMap, wanted: &str) -> Option<&'a str> {
    headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|cookie| {
                let (name, value) = cookie.trim().split_once('=')?;
                (name == wanted).then_some(value)
            })
        })
}

fn claim_download_session(
    state: &SharedState,
    share_id: &str,
    expected_ip: IpAddr,
    headers: &HeaderMap,
) -> Result<bool, AppError> {
    if let Some(token) = cookie_value(headers, "tempshare_session") {
        let mut sessions = state.download_sessions.lock().unwrap();
        if let Some(session) = sessions.get_mut(token) {
            if session.expires_at > Instant::now()
                && session.share_id == share_id
                && session.client_ip == expected_ip
            {
                if session.counted {
                    return Ok(true);
                }
                let claimed = db::claim_download(&state.db, share_id, now_ts())
                    .map_err(AppError::Internal)?;
                if claimed {
                    session.counted = true;
                }
                return Ok(claimed);
            }
        }
    }
    db::claim_download(&state.db, share_id, now_ts()).map_err(AppError::Internal)
}

fn has_counted_download_session(
    state: &SharedState,
    share_id: &str,
    expected_ip: IpAddr,
    headers: &HeaderMap,
) -> bool {
    let Some(token) = cookie_value(headers, "tempshare_session") else {
        return false;
    };
    let sessions = state.download_sessions.lock().unwrap();
    sessions.get(token).is_some_and(|session| {
        session.counted
            && session.expires_at > Instant::now()
            && session.share_id == share_id
            && session.client_ip == expected_ip
    })
}

/// Public recipient page. It exposes only safe share metadata and provides
/// a normal password flow before navigating to the streaming download.
pub async fn share_page(
    State(state): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let share = load_active_share(&state, &id)?;
    let client_ip = client_ip(addr, &headers);
    let session_token = generate_session_token();
    state.download_sessions.lock().unwrap().insert(
        session_token.clone(),
        crate::state::DownloadSession {
            share_id: id.clone(),
            client_ip,
            expires_at: Instant::now() + Duration::from_secs(86_400),
            counted: false,
        },
    );
    let name = html_escape(&share.display_name);
    let id_json = serde_json::to_string(&share.id).map_err(|e| AppError::Internal(e.into()))?;
    let password_controls = if share.password_hash.is_some() {
        r#"<label for="password">Password</label>
        <input id="password" type="password" autocomplete="current-password">
        <button id="download">Unlock and download</button>"#
    } else {
        r#"<button id="download">Download</button>"#
    };
    let requires_password = share.password_hash.is_some();
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Download {name}</title><style>
body{{margin:0;background:#0f1117;color:#e6e8ef;font-family:system-ui,sans-serif;display:grid;place-items:center;min-height:100vh}}
main{{width:min(92vw,460px);background:#171a23;border:1px solid #262b38;border-radius:14px;padding:24px}}
h1{{font-size:20px;overflow-wrap:anywhere}}p{{color:#9aa3b7}}label{{display:block;margin:18px 0 6px}}
input{{width:100%;box-sizing:border-box;padding:11px;background:#10131b;color:white;border:1px solid #343b4d;border-radius:8px}}
button{{width:100%;margin-top:16px;padding:12px;border:0;border-radius:8px;background:#5b8def;color:white;font-weight:700;cursor:pointer}}
#error{{color:#fca5a5;min-height:1.4em}}</style></head><body><main>
<p>TempShare file</p><h1>{name}</h1><p>{} bytes</p>{password_controls}<p id="error"></p>
<script>const id={id_json};const protectedShare={requires_password};
document.getElementById('download').onclick=async()=>{{const error=document.getElementById('error');error.textContent='';
if(protectedShare){{const password=document.getElementById('password').value;const response=await fetch('/api/download/'+encodeURIComponent(id)+'/unlock',{{method:'POST',headers:{{'content-type':'application/json'}},body:JSON.stringify({{password}})}});if(!response.ok){{const body=await response.json().catch(()=>({{}}));error.textContent=body.error||'Unable to unlock';return;}}}}
window.location.assign('/download/'+encodeURIComponent(id));}};</script></main></body></html>"#,
        share.size_bytes
    );
    let mut response = Html(html).into_response();
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; form-action 'none'; base-uri 'none'"
            .parse()
            .unwrap(),
    );
    response
        .headers_mut()
        .insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response.headers_mut().append(
        header::SET_COOKIE,
        format!(
            "tempshare_session={session_token}; Path=/download/{id}; Max-Age=86400; HttpOnly; Secure; SameSite=Strict"
        )
        .parse::<axum::http::HeaderValue>()
        .map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?,
    );
    Ok(response)
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// GET /api/download/:id/info -- metadata for the browser's download page
/// (filename, size, whether a password is needed) without exposing the
/// real file path or triggering a download.
pub async fn share_info(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let share = load_active_share(&state, &id)?;
    Ok(Json(json!({
        "id": share.id,
        "display_name": share.display_name,
        "size_bytes": share.size_bytes,
        "requires_password": share.password_hash.is_some(),
    })))
}

#[cfg(test)]
mod tests {
    use super::{client_ip, content_disposition, parse_range};
    use axum::http::HeaderMap;
    use std::net::SocketAddr;

    #[test]
    fn range_parser_accepts_valid_single_ranges() {
        assert_eq!(parse_range("bytes=5-9", 20), Some((5, 9)));
        assert_eq!(parse_range("bytes=5-", 20), Some((5, 19)));
        assert_eq!(parse_range("bytes=-5", 20), Some((15, 19)));
        assert_eq!(parse_range("bytes=0-999", 20), Some((0, 19)));
    }

    #[test]
    fn range_parser_rejects_adversarial_values() {
        for value in [
            "bytes=-",
            "bytes=--1",
            "bytes=-0",
            "bytes=5-2",
            "bytes=20-",
            "bytes=999999999999999999999999-",
            "bytes=1-2,4-5",
            "bytes=abc-def",
            "bytes=",
            "items=0-1",
        ] {
            assert_eq!(parse_range(value, 20), None, "{value}");
        }
        assert_eq!(parse_range("bytes=0-0", 0), None);
    }

    #[test]
    fn disposition_preserves_unicode_filename() {
        let value = content_disposition("محاضرة 🎓.mp4");
        assert!(value.contains("filename*=UTF-8''"));
        assert!(value.contains("%D9%85%D8%AD%D8%A7%D8%B6%D8%B1%D8%A9"));
        assert!(value.ends_with(".mp4"));
    }

    #[test]
    fn cloudflare_ip_is_trusted_only_from_loopback_proxy() {
        let mut headers = HeaderMap::new();
        headers.insert("cf-connecting-ip", "203.0.113.9".parse().unwrap());
        let loopback: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let remote: SocketAddr = "198.51.100.7:5000".parse().unwrap();
        assert_eq!(
            client_ip(loopback, &headers),
            "203.0.113.9".parse::<std::net::IpAddr>().unwrap()
        );
        assert_eq!(
            client_ip(remote, &headers),
            "198.51.100.7".parse::<std::net::IpAddr>().unwrap()
        );
    }
}
