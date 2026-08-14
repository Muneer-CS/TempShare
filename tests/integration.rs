use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tempshare::state::{AppState, Config};
use tempshare::{db, management_router, public_router};
use tower::util::ServiceExt;

fn test_state(dir: &std::path::Path) -> tempshare::state::SharedState {
    let db_path = dir.join("test.db");
    let storage_dir = dir.join("storage");
    std::fs::create_dir_all(&storage_dir).unwrap();
    let pool = db::init_pool(db_path.to_str().unwrap()).unwrap();
    let config = Config {
        bind_addr: "127.0.0.1:0".to_string(),
        public_bind_addr: "127.0.0.1:0".to_string(),
        public_base_url: "http://127.0.0.1:7421".to_string(),
        auto_tunnel: false,
        storage_dir,
        db_path: db_path.to_str().unwrap().to_string(),
        max_upload_bytes: 1024 * 1024,
        global_rate_limit_per_min: 1000,
        failed_auth_max: 5,
        failed_auth_window_secs: 300,
        unlock_token_ttl_secs: 900,
        bandwidth_bytes_per_sec: 0,
    };
    AppState::new(pool, config)
}

fn multipart_body(boundary: &str, fields: &[(&str, &str)], file: Option<(&str, &[u8])>) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    if let Some((filename, content)) = file {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(content);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

#[tokio::test]
async fn create_and_download_full_file() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    let app = management_router(state.clone());

    let boundary = "X-BOUNDARY";
    let content = b"hello tempshare, this is the file body";
    let body = multipart_body(
        boundary,
        &[
            ("expires", "never"),
            ("max_downloads", "unlimited"),
            ("password", ""),
        ],
        Some(("greeting.txt", content)),
    );

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    9999,
                ))))
                .method("POST")
                .uri("/api/shares")
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let download_url = json["download_url"].as_str().unwrap().to_string();

    // Download the full file back and check bytes match exactly.
    let res = app
        .oneshot(
            Request::builder()
                .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    9999,
                ))))
                .method("GET")
                .uri(&download_url)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers().get(header::ACCEPT_RANGES).unwrap(), "bytes");
    let downloaded = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&downloaded[..], content);
    let conn = state.db.get().unwrap();
    let (recorded_bytes, completed): (i64, i64) = conn
        .query_row(
            "SELECT bytes_transferred, completed FROM download_events ORDER BY id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(recorded_bytes, content.len() as i64);
    assert_eq!(completed, 1);
}

#[tokio::test]
async fn range_request_returns_partial_content() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    let app = management_router(state.clone());

    let boundary = "X-BOUNDARY";
    let content = b"0123456789ABCDEFGHIJ"; // 20 bytes
    let body = multipart_body(
        boundary,
        &[
            ("expires", "never"),
            ("max_downloads", "unlimited"),
            ("password", ""),
        ],
        Some(("data.bin", content)),
    );
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    9999,
                ))))
                .method("POST")
                .uri("/api/shares")
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let download_url = json["download_url"].as_str().unwrap().to_string();

    // Ask for bytes 5-9 inclusive ("56789").
    let res = app
        .oneshot(
            Request::builder()
                .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    9999,
                ))))
                .method("GET")
                .uri(&download_url)
                .header(header::RANGE, "bytes=5-9")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        res.headers().get(header::CONTENT_RANGE).unwrap(),
        "bytes 5-9/20"
    );
    let partial = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&partial[..], b"56789");
}

#[tokio::test]
async fn nonexistent_share_returns_404_not_500() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    let app = public_router(state.clone());

    let res = app
        .oneshot(
            Request::builder()
                .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    9999,
                ))))
                .method("GET")
                .uri("/download/this-id-was-never-issued-xyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn password_protected_share_requires_unlock() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    let app = management_router(state.clone());

    let boundary = "X-BOUNDARY";
    let content = b"top secret payload";
    let body = multipart_body(
        boundary,
        &[
            ("expires", "never"),
            ("max_downloads", "unlimited"),
            ("password", "correct-horse"),
        ],
        Some(("secret.txt", content)),
    );
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    9999,
                ))))
                .method("POST")
                .uri("/api/shares")
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let download_url = json["download_url"].as_str().unwrap().to_string();

    // Without unlocking: 401.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    9999,
                ))))
                .method("GET")
                .uri(&download_url)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // Wrong password: 401, and it must not leak whether the share exists
    // differently than a correct-but-wrong-id request would.
    let share_id = download_url.trim_start_matches("/download/");
    let page = app
        .clone()
        .oneshot(
            Request::builder()
                .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    9999,
                ))))
                .uri(format!("/s/{share_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert!(page.headers().contains_key(header::SET_COOKIE));
    let page_html = page.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&page_html).contains("Unlock and download"));
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    9999,
                ))))
                .method("POST")
                .uri(format!("/api/download/{share_id}/unlock"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"password":"wrong"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // Correct password unlocks and returns a token usable for the download.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    9999,
                ))))
                .method("POST")
                .uri(format!("/api/download/{share_id}/unlock"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"password":"correct-horse"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.headers().contains_key(header::SET_COOKIE));
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let token = json["token"].as_str().unwrap().to_string();

    let res = app
        .oneshot(
            Request::builder()
                .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    9999,
                ))))
                .method("GET")
                .uri(&download_url)
                .header("x-tempshare-token", token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn revoked_share_is_unreachable() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    let app = management_router(state.clone());

    let boundary = "X-BOUNDARY";
    let body = multipart_body(
        boundary,
        &[
            ("expires", "never"),
            ("max_downloads", "unlimited"),
            ("password", ""),
        ],
        Some(("f.txt", b"data")),
    );
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    9999,
                ))))
                .method("POST")
                .uri("/api/shares")
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = json["id"].as_str().unwrap().to_string();

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    9999,
                ))))
                .method("POST")
                .uri(format!("/api/shares/{id}/revoke"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let res = app
        .oneshot(
            Request::builder()
                .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    9999,
                ))))
                .method("GET")
                .uri(format!("/download/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn upload_exceeding_max_size_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path()); // max_upload_bytes = 1MB
    let app = management_router(state.clone());

    let boundary = "X-BOUNDARY";
    let big = vec![0u8; 2 * 1024 * 1024]; // 2MB > 1MB cap
    let body = multipart_body(
        boundary,
        &[
            ("expires", "never"),
            ("max_downloads", "unlimited"),
            ("password", ""),
        ],
        Some(("big.bin", &big)),
    );
    let res = app
        .oneshot(
            Request::builder()
                .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    9999,
                ))))
                .method("POST")
                .uri("/api/shares")
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn public_router_cannot_reach_management_routes() {
    let dir = tempfile::tempdir().unwrap();
    let app = public_router(test_state(dir.path()));
    for (method, uri) in [
        ("GET", "/api/shares"),
        ("POST", "/api/shares"),
        ("PATCH", "/api/shares/anything"),
        ("POST", "/api/shares/anything/revoke"),
        ("DELETE", "/api/shares/anything"),
        ("GET", "/api/status"),
        ("GET", "/"),
    ] {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{method} {uri}");
    }
}

#[tokio::test]
async fn malformed_ranges_are_rejected_instead_of_serving_full_file() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    let path = state.config.storage_dir.join("range-id");
    std::fs::write(&path, b"0123456789").unwrap();
    db::insert_share(
        &state.db,
        &db::Share {
            id: "range-id".into(),
            display_name: "data.bin".into(),
            file_path: path.to_string_lossy().into_owned(),
            size_bytes: 10,
            is_folder: false,
            created_at: chrono::Utc::now().timestamp(),
            expires_at: None,
            max_downloads: None,
            download_count: 0,
            password_hash: None,
            status: "active".into(),
        },
    )
    .unwrap();
    let app = public_router(state);

    for value in [
        "bytes=5-2",
        "bytes=-",
        "bytes=999999999999999999999999-",
        "bytes=1-2,4-5",
        "bytes=abc-def",
    ] {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                        [127, 0, 0, 1],
                        9999,
                    ))))
                    .uri("/download/range-id")
                    .header(header::RANGE, value)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::RANGE_NOT_SATISFIABLE, "{value}");
    }
}

#[tokio::test]
async fn folder_share_downloads_as_zip_with_safe_opaque_storage() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    let app = management_router(state.clone());
    let boundary = "FOLDER-BOUNDARY";
    let mut body = Vec::new();
    for (filename, content) in [
        ("docs/readme.txt", &b"folder readme"[..]),
        ("docs/nested/data.bin", &b"\x00\x01\x02"[..]),
    ] {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(content);
        body.extend_from_slice(b"\r\n");
    }
    for (name, value) in [
        ("folder_name", "docs"),
        ("expires", "never"),
        ("max_downloads", "unlimited"),
        ("password", ""),
    ] {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/shares/folder")
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let id = json["id"].as_str().unwrap();
    let share = db::get_share(&state.db, id).unwrap().unwrap();
    assert!(share.is_folder);
    assert!(std::path::Path::new(&share.file_path).is_dir());
    for entry in db::list_share_entries(&state.db, id).unwrap() {
        assert!(!entry.stored_name.contains(['/', '\\']));
    }

    let response = app
        .oneshot(
            Request::builder()
                .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    9999,
                ))))
                .uri(format!("/download/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/zip");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let reader = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(reader).unwrap();
    let mut readme = String::new();
    std::io::Read::read_to_string(
        &mut archive.by_name("docs/readme.txt").unwrap(),
        &mut readme,
    )
    .unwrap();
    assert_eq!(readme, "folder readme");
    let mut data = Vec::new();
    std::io::Read::read_to_end(
        &mut archive.by_name("docs/nested/data.bin").unwrap(),
        &mut data,
    )
    .unwrap();
    assert_eq!(data, b"\x00\x01\x02");
}

#[tokio::test]
async fn qr_code_assets_are_served_locally() {
    let dir = tempfile::tempdir().unwrap();
    let app = management_router(test_state(dir.path()));
    for uri in ["/qrcode.min.js", "/app.js"] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
    }
}

#[tokio::test]
async fn management_router_rejects_dns_rebinding_host_and_foreign_origin() {
    let dir = tempfile::tempdir().unwrap();
    let app = management_router(test_state(dir.path()));
    for request in [
        Request::builder()
            .uri("/api/shares")
            .header(header::HOST, "attacker.example")
            .body(Body::empty())
            .unwrap(),
        Request::builder()
            .method("POST")
            .uri("/api/shares")
            .header(header::HOST, "127.0.0.1:7420")
            .header(header::ORIGIN, "https://attacker.example")
            .body(Body::empty())
            .unwrap(),
    ] {
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}

#[tokio::test]
async fn one_recipient_session_can_resume_without_consuming_extra_downloads() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    let path = state.config.storage_dir.join("resume-id");
    std::fs::write(&path, b"0123456789").unwrap();
    db::insert_share(
        &state.db,
        &db::Share {
            id: "resume-id".into(),
            display_name: "محاضرة.mp4".into(),
            file_path: path.to_string_lossy().into_owned(),
            size_bytes: 10,
            is_folder: false,
            created_at: chrono::Utc::now().timestamp(),
            expires_at: None,
            max_downloads: Some(1),
            download_count: 0,
            password_hash: None,
            status: "active".into(),
        },
    )
    .unwrap();
    let app = public_router(state.clone());
    let page = app
        .clone()
        .oneshot(
            Request::builder()
                .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    9999,
                ))))
                .uri("/s/resume-id")
                .header("cf-connecting-ip", "203.0.113.10")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    let session_cookie = page.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();

    for (range, expected) in [("bytes=0-4", &b"01234"[..]), ("bytes=5-9", &b"56789"[..])] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                        [127, 0, 0, 1],
                        9999,
                    ))))
                    .uri("/download/resume-id")
                    .header("cf-connecting-ip", "203.0.113.10")
                    .header(header::COOKIE, &session_cookie)
                    .header(header::RANGE, range)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..], expected);
    }
    assert_eq!(
        db::get_share(&state.db, "resume-id")
            .unwrap()
            .unwrap()
            .download_count,
        1
    );

    let new_recipient = app
        .oneshot(
            Request::builder()
                .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    9998,
                ))))
                .uri("/download/resume-id")
                .header("cf-connecting-ip", "203.0.113.11")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(new_recipient.status(), StatusCode::NOT_FOUND);
}
