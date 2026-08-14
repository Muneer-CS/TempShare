//! Handlers for creating and managing shares. These are the endpoints the
//! *local* UI talks to (create/list/revoke/delete/update). They are bound
//! only to the loopback interface by default (see main.rs) since they are
//! management operations, not public download endpoints.

use axum::extract::{Multipart, Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use std::io::Write;
use tokio::io::AsyncWriteExt;

use crate::db::{self, Share, ShareSummary};
use crate::error::AppError;
use crate::ids::generate_share_id;
use crate::state::SharedState;

struct PendingUpload(Option<std::path::PathBuf>);

impl Drop for PendingUpload {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

struct PendingFolder(Option<std::path::PathBuf>);

impl Drop for PendingFolder {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Parses a duration keyword ("1h", "24h", "7d", "30d", "never", or a raw
/// number of seconds) into an absolute expiry timestamp.
fn parse_expiry(input: &str) -> Result<Option<i64>, AppError> {
    let now = now_ts();
    let secs: Option<i64> = match input {
        "never" | "" => None,
        "1h" => Some(3600),
        "24h" => Some(86_400),
        "7d" => Some(7 * 86_400),
        "30d" => Some(30 * 86_400),
        other => Some(
            other
                .parse::<i64>()
                .map_err(|_| AppError::BadRequest("invalid expiry value".into()))?,
        ),
    };
    if secs.is_some_and(|s| s <= 0) {
        return Err(AppError::BadRequest("expiry must be positive".into()));
    }
    secs.map(|s| {
        now.checked_add(s)
            .ok_or_else(|| AppError::BadRequest("expiry is too large".into()))
    })
    .transpose()
}

fn parse_max_downloads(input: &str) -> Result<Option<i64>, AppError> {
    match input {
        "unlimited" | "" => Ok(None),
        other => {
            let value = other
                .parse::<i64>()
                .map_err(|_| AppError::BadRequest("invalid max_downloads value".into()))?;
            if value <= 0 {
                return Err(AppError::BadRequest(
                    "max_downloads must be positive".into(),
                ));
            }
            Ok(Some(value))
        }
    }
}

pub async fn create_share(
    State(state): State<SharedState>,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, AppError> {
    std::fs::create_dir_all(&state.config.storage_dir).map_err(|e| AppError::Internal(e.into()))?;

    let share_id = generate_share_id();
    let mut original_name: Option<String> = None;
    let mut size_bytes: i64 = 0;
    let mut expiry_field = "never".to_string();
    let mut max_downloads_field = "unlimited".to_string();
    let mut password: Option<String> = None;
    let mut dest_path: Option<std::path::PathBuf> = None;
    let mut pending_upload = PendingUpload(None);

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| AppError::BadRequest("malformed upload".into()))?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                if dest_path.is_some() {
                    if let Some(path) = &dest_path {
                        let _ = tokio::fs::remove_file(path).await;
                    }
                    return Err(AppError::BadRequest(
                        "only one file may be uploaded per share".into(),
                    ));
                }
                let mut field = field;
                let filename = field
                    .file_name()
                    .map(sanitize_filename)
                    .unwrap_or_else(|| "download.bin".to_string());
                original_name = Some(filename.clone());

                // Store under the opaque share ID, never the user-supplied
                // filename, so nothing on disk is guessable/collidable and
                // no path-manipulation in the filename can escape the
                // storage directory.
                let path = state.config.storage_dir.join(&share_id);
                let mut file = tokio::fs::File::create(&path)
                    .await
                    .map_err(|e| AppError::Internal(e.into()))?;
                pending_upload.0 = Some(path.clone());

                while let Some(chunk) = field
                    .chunk()
                    .await
                    .map_err(|_| AppError::BadRequest("upload interrupted".into()))?
                {
                    size_bytes = size_bytes
                        .checked_add(i64::try_from(chunk.len()).map_err(|_| {
                            AppError::BadRequest("file exceeds maximum allowed size".into())
                        })?)
                        .ok_or_else(|| {
                            AppError::BadRequest("file exceeds maximum allowed size".into())
                        })?;
                    if size_bytes as u64 > state.config.max_upload_bytes {
                        let _ = tokio::fs::remove_file(&path).await;
                        return Err(AppError::BadRequest(
                            "file exceeds maximum allowed size".into(),
                        ));
                    }
                    file.write_all(&chunk)
                        .await
                        .map_err(|e| AppError::Internal(e.into()))?;
                }
                file.flush()
                    .await
                    .map_err(|e| AppError::Internal(e.into()))?;
                dest_path = Some(path);
            }
            "expires" => {
                expiry_field = field_text(field).await?;
            }
            "max_downloads" => {
                max_downloads_field = field_text(field).await?;
            }
            "password" => {
                let v = field_text(field).await?;
                if !v.is_empty() {
                    password = Some(v);
                }
            }
            _ => {
                // Unknown field: drain and ignore rather than erroring, so
                // the API tolerates future optional fields gracefully.
                let _ = field_text(field).await;
            }
        }
    }

    let dest_path = dest_path.ok_or_else(|| AppError::BadRequest("no file provided".into()))?;
    let display_name = original_name.unwrap_or_else(|| "download.bin".to_string());
    let expires_at = match parse_expiry(&expiry_field) {
        Ok(v) => v,
        Err(e) => {
            let _ = tokio::fs::remove_file(&dest_path).await;
            return Err(e);
        }
    };
    let max_downloads = match parse_max_downloads(&max_downloads_field) {
        Ok(v) => v,
        Err(e) => {
            let _ = tokio::fs::remove_file(&dest_path).await;
            return Err(e);
        }
    };
    let password_hash = match password {
        Some(p) => Some(crate::auth::hash_password(&p).map_err(AppError::Internal)?),
        None => None,
    };

    let share = Share {
        id: share_id.clone(),
        display_name,
        file_path: dest_path.to_string_lossy().to_string(),
        size_bytes,
        is_folder: false,
        created_at: now_ts(),
        expires_at,
        max_downloads,
        download_count: 0,
        password_hash,
        status: "active".to_string(),
    };

    if let Err(e) = db::insert_share(&state.db, &share) {
        let _ = tokio::fs::remove_file(&dest_path).await;
        return Err(AppError::Internal(e));
    }
    pending_upload.0 = None;

    Ok(Json(json!({
        "id": share.id,
        "download_url": format!("/download/{}", share.id),
        "public_download_url": format!("{}/s/{}", state.public_base_url(), share.id),
    })))
}

/// Creates a folder share from repeated `file` multipart fields. Files are
/// stored under server-generated opaque names. Client-provided relative
/// names are retained only as ZIP entry metadata.
pub async fn create_folder_share(
    State(state): State<SharedState>,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, AppError> {
    std::fs::create_dir_all(&state.config.storage_dir).map_err(|e| AppError::Internal(e.into()))?;
    let share_id = generate_share_id();
    let folder_path = state.config.storage_dir.join(&share_id);
    tokio::fs::create_dir(&folder_path)
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    let mut pending = PendingFolder(Some(folder_path.clone()));
    let mut entries = Vec::new();
    let mut total_size = 0i64;
    let mut expiry_field = "never".to_string();
    let mut max_downloads_field = "unlimited".to_string();
    let mut password: Option<String> = None;
    let mut folder_name = "shared-folder".to_string();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| AppError::BadRequest("malformed upload".into()))?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                let mut field = field;
                let display_name = field
                    .file_name()
                    .map(sanitize_zip_entry)
                    .unwrap_or_else(|| "file.bin".to_string());
                let stored_name = generate_share_id();
                let path = folder_path.join(&stored_name);
                let mut file = tokio::fs::File::create(&path)
                    .await
                    .map_err(|e| AppError::Internal(e.into()))?;
                let mut entry_size = 0i64;
                while let Some(chunk) = field
                    .chunk()
                    .await
                    .map_err(|_| AppError::BadRequest("upload interrupted".into()))?
                {
                    let len = i64::try_from(chunk.len()).map_err(|_| {
                        AppError::BadRequest("folder exceeds maximum allowed size".into())
                    })?;
                    entry_size = entry_size.checked_add(len).ok_or_else(|| {
                        AppError::BadRequest("folder exceeds maximum allowed size".into())
                    })?;
                    total_size = total_size.checked_add(len).ok_or_else(|| {
                        AppError::BadRequest("folder exceeds maximum allowed size".into())
                    })?;
                    if total_size as u64 > state.config.max_upload_bytes {
                        return Err(AppError::BadRequest(
                            "folder exceeds maximum allowed size".into(),
                        ));
                    }
                    file.write_all(&chunk)
                        .await
                        .map_err(|e| AppError::Internal(e.into()))?;
                }
                file.flush()
                    .await
                    .map_err(|e| AppError::Internal(e.into()))?;
                entries.push(db::ShareEntry {
                    stored_name,
                    display_name,
                    size_bytes: entry_size,
                });
            }
            "folder_name" => folder_name = sanitize_filename(&field_text(field).await?),
            "expires" => expiry_field = field_text(field).await?,
            "max_downloads" => max_downloads_field = field_text(field).await?,
            "password" => {
                let value = field_text(field).await?;
                if !value.is_empty() {
                    password = Some(value);
                }
            }
            _ => {
                let _ = field_text(field).await;
            }
        }
    }
    if entries.is_empty() {
        return Err(AppError::BadRequest("no files provided".into()));
    }
    let expires_at = parse_expiry(&expiry_field)?;
    let max_downloads = parse_max_downloads(&max_downloads_field)?;
    let password_hash = password
        .map(|value| crate::auth::hash_password(&value))
        .transpose()
        .map_err(AppError::Internal)?;
    if !folder_name.to_ascii_lowercase().ends_with(".zip") {
        folder_name.push_str(".zip");
    }
    let share = Share {
        id: share_id.clone(),
        display_name: folder_name,
        file_path: folder_path.to_string_lossy().into_owned(),
        size_bytes: total_size,
        is_folder: true,
        created_at: now_ts(),
        expires_at,
        max_downloads,
        download_count: 0,
        password_hash,
        status: "active".into(),
    };
    db::insert_folder_share(&state.db, &share, &entries).map_err(AppError::Internal)?;
    pending.0 = None;
    Ok(Json(json!({
        "id": share.id,
        "download_url": format!("/download/{}", share.id),
        "public_download_url": format!("{}/s/{}", state.public_base_url(), share.id),
    })))
}

pub async fn server_status(State(state): State<SharedState>) -> Json<serde_json::Value> {
    Json(json!({
        "public_base_url": state.public_base_url(),
        "tunnel_status": state.tunnel_status(),
        "auto_tunnel": state.config.auto_tunnel,
    }))
}

async fn field_text(field: axum::extract::multipart::Field<'_>) -> Result<String, AppError> {
    let bytes = field
        .bytes()
        .await
        .map_err(|_| AppError::BadRequest("malformed field".into()))?;
    Ok(String::from_utf8_lossy(&bytes).trim().to_string())
}

/// Strips path separators and control characters from a client-supplied
/// filename. This filename is only ever used for display / the
/// Content-Disposition header -- never to build a filesystem path (the
/// share ID is used for that) -- but we sanitize it anyway as defense in
/// depth against header injection and confusing UI display.
fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "download.bin".to_string()
    } else {
        // Guard against absolute-path-looking or traversal-looking names
        // even though they're never used as paths -- keeps things sane.
        cleaned.replace("..", "_").chars().take(255).collect()
    }
}

fn sanitize_zip_entry(name: &str) -> String {
    let parts: Vec<String> = name
        .split(['/', '\\'])
        .filter(|part| !part.is_empty() && *part != "." && *part != "..")
        .map(sanitize_filename)
        .collect();
    if parts.is_empty() {
        "file.bin".to_string()
    } else {
        parts.join("/")
    }
}

pub async fn list_shares(
    State(state): State<SharedState>,
) -> Result<Json<Vec<ShareSummary>>, AppError> {
    let shares = db::list_shares(&state.db).map_err(AppError::Internal)?;
    Ok(Json(shares.iter().map(ShareSummary::from).collect()))
}

#[derive(Deserialize)]
pub struct UpdateShareRequest {
    pub expires: Option<String>,
    pub max_downloads: Option<String>,
    pub password: Option<String>, // empty string = remove password
}

pub async fn update_share(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateShareRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    db::get_share(&state.db, &id)
        .map_err(AppError::Internal)?
        .ok_or(AppError::NotFound)?;

    let expires_at = match body.expires {
        Some(e) => Some(parse_expiry(&e)?),
        None => None,
    };
    let max_downloads = match body.max_downloads {
        Some(m) => Some(parse_max_downloads(&m)?),
        None => None,
    };
    let password_hash = match body.password {
        Some(p) if p.is_empty() => Some(None),
        Some(p) => Some(Some(
            crate::auth::hash_password(&p).map_err(AppError::Internal)?,
        )),
        None => None,
    };

    db::update_share_settings(&state.db, &id, expires_at, max_downloads, password_hash)
        .map_err(AppError::Internal)?;

    Ok(Json(json!({ "ok": true })))
}

pub async fn revoke_share(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let ok = db::revoke_share(&state.db, &id).map_err(AppError::Internal)?;
    if !ok {
        return Err(AppError::NotFound);
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct DeleteQuery {
    #[serde(default)]
    pub delete_file: bool,
}

pub async fn delete_share(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<DeleteQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let share = db::get_share(&state.db, &id)
        .map_err(AppError::Internal)?
        .ok_or(AppError::NotFound)?;

    db::delete_share(&state.db, &id).map_err(AppError::Internal)?;

    if q.delete_file {
        // Explicit user action only -- deleting a share record never
        // deletes the underlying file unless the caller opts in.
        if share.is_folder {
            let _ = std::fs::remove_dir_all(&share.file_path);
        } else {
            let _ = std::fs::remove_file(&share.file_path);
        }
    }

    Ok(Json(json!({ "ok": true })))
}

#[allow(dead_code)]
fn _write_placeholder(mut w: impl Write) {
    let _ = w.write_all(b"");
}
