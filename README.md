# TempShare

TempShare is a local-first temporary file-sharing service written in Rust. It gives a sender a private local dashboard, creates expiring share links, and streams files directly from the sender's computer to a recipient through a browser.

The optional Windows distribution can bundle `cloudflared` to create an account-free HTTPS Quick Tunnel. Recipients do not install TempShare: the same link works in modern desktop and mobile browsers.

## Highlights

- Separate management and public-download listeners
- File and folder sharing with automatic ZIP generation for folders
- Expiration, revocation, maximum-download, and password controls
- Argon2id password hashing and per-IP abuse protection
- HTTP Range support for resumable and partial downloads
- Streaming I/O instead of loading complete files into memory
- Unicode filename support with standards-compliant `Content-Disposition`
- Mobile-friendly recipient page and locally generated QR codes
- Automatic Cloudflare Quick Tunnel discovery and restart on Windows
- SQLite persistence with additive schema migration
- Automated unit and integration tests

## Architecture

TempShare deliberately separates local administration from public downloads:

| Service | Default address | Purpose |
| --- | --- | --- |
| Management | `127.0.0.1:7420` | Dashboard and create/list/update/revoke/delete operations |
| Public download | `127.0.0.1:7421` | Recipient page, password unlock, and file downloads |

Only the public download service should be connected to a tunnel or reverse proxy. Never expose the management listener to the internet.

```text
Browser dashboard (sender)
        |
        v
127.0.0.1:7420  management API

Recipient browser -> HTTPS tunnel -> 127.0.0.1:7421 -> streamed file
```

## Technology

- Rust 2021
- Tokio and Axum
- SQLite through `r2d2_sqlite`
- Argon2id password hashing
- Vanilla HTML, CSS, and JavaScript
- QRCode.js (bundled locally)
- Optional Cloudflare Tunnel client for public HTTPS links

## Repository layout

```text
src/
  auth.rs       Password hashing and request limiting
  db.rs         SQLite schema and share registry
  download.rs   Recipient page, unlock flow, Range streaming, accounting
  error.rs      HTTP-safe application errors
  ids.rs        Cryptographically secure identifiers and tokens
  lib.rs        Management/public routers and middleware
  main.rs       Listeners, cleanup task, tunnel lifecycle, Windows wake lock
  shares.rs     Share creation, updates, revocation, and deletion
  state.rs      Configuration and shared application state
static/         Dashboard assets and local QR generator
tests/          End-to-end router tests
```

## Prerequisites

- A current stable Rust toolchain
- On Windows, Visual Studio Build Tools with **Desktop development with C++**
- Optional: `cloudflared` for public HTTPS sharing

## Build from source

```bash
git clone <your-repository-url>
cd tempshare-secure-file-sharing
cp .env.example .env
cargo build --release
cargo test --release
```

On PowerShell, copy the environment template with:

```powershell
Copy-Item .env.example .env
```

The release binary is written to `target/release/tempshare` (`tempshare.exe` on Windows).

### Windows build helper

```powershell
powershell -ExecutionPolicy Bypass -File .\build-windows.ps1
```

Keep the `static` directory beside the executable when assembling a release package.

## Run locally

```bash
cargo run --release
```

Open `http://127.0.0.1:7420`. With the default source configuration, TempShare attempts to launch `cloudflared` only when `TEMPSHARE_AUTO_TUNNEL=true` and the executable is available beside TempShare.

To run without a tunnel, set this in `.env`:

```dotenv
TEMPSHARE_AUTO_TUNNEL=false
```

The public service is then available locally at `http://127.0.0.1:7421`.

## Optional public links with Cloudflare Quick Tunnel

1. Download the Windows `cloudflared` executable from Cloudflare's official releases.
2. Rename it to `cloudflared.exe` and place it beside `tempshare.exe`.
3. Keep `cloudflared-quick.yml` beside the executable.
4. Set `TEMPSHARE_AUTO_TUNNEL=true`.
5. Start TempShare and wait for the dashboard to report that secure public sharing is ready.

Quick Tunnel addresses are temporary, change after restarts, and have no uptime guarantee. They are suitable for occasional sharing, not production hosting. A stable address requires a named tunnel and a domain.

## Usage

1. Start TempShare and keep its terminal window open.
2. Wait for the public URL readiness message if using a tunnel.
3. Select a file or folder in the local dashboard.
4. Configure expiration, maximum downloads, and an optional password.
5. Send the generated link or QR code to the recipient.
6. Revoke or delete the share when the transfer is complete.

Only the sender runs the application. Recipients need only a modern browser.

## Configuration

All runtime settings are documented in `.env.example`.

| Variable | Default | Description |
| --- | --- | --- |
| `TEMPSHARE_BIND_ADDR` | `127.0.0.1:7420` | Local management listener |
| `TEMPSHARE_PUBLIC_BIND_ADDR` | `127.0.0.1:7421` | Public download listener |
| `TEMPSHARE_PUBLIC_BASE_URL` | `http://127.0.0.1:7421` | Link origin when automatic tunneling is disabled |
| `TEMPSHARE_AUTO_TUNNEL` | `true` | Start a bundled Quick Tunnel client |
| `TEMPSHARE_STORAGE_DIR` | `./data/shared_files` | Local uploaded-file storage |
| `TEMPSHARE_DB_PATH` | `./data/tempshare.db` | SQLite database path |
| `TEMPSHARE_MAX_UPLOAD_BYTES` | `53687091200` | Per-share upload cap in bytes |
| `TEMPSHARE_RATE_LIMIT_PER_MIN` | `120` | Public per-IP request budget |
| `TEMPSHARE_UNLOCK_TOKEN_TTL_SECS` | `900` | Password unlock lifetime |
| `TEMPSHARE_BANDWIDTH_BYTES_PER_SEC` | `0` | Per-connection bandwidth cap; `0` is unlimited |

## Docker

Build the image:

```bash
docker build -t tempshare .
```

Run it without automatic tunneling. The management port is explicitly published only on the host loopback interface:

```bash
docker run --rm \
  -e TEMPSHARE_AUTO_TUNNEL=false \
  -p 127.0.0.1:7420:7420 \
  -p 127.0.0.1:7421:7421 \
  -v tempshare-data:/data \
  tempshare
```

Do not change the management mapping to `0.0.0.0:7420` on an internet-facing host.

## Security model

- Share IDs are generated with a cryptographically secure random number generator.
- No public route accepts a client-provided filesystem path.
- Passwords are stored as Argon2id hashes, never plaintext.
- Expiration, status, and download limits are checked server-side.
- Management requests reject non-local host values and foreign browser origins.
- Cloudflare client IP headers are trusted only from a loopback tunnel connection.
- Unlock and download-session tokens are bound to the recipient IP.
- Public downloads use `Cache-Control: no-store` and restrictive browser headers.

See [SECURITY.md](SECURITY.md) for deployment guidance and vulnerability reporting.

## Local storage and privacy

Browser uploads create a working copy under `data/shared_files` because browser pages cannot provide the original filesystem path. Folder downloads are assembled into a temporary ZIP and removed when streaming ends.

The default Quick Tunnel avoids permanent cloud storage, but Cloudflare terminates HTTPS and processes traffic in transit. TempShare is therefore not end-to-end encrypted against the tunnel provider.

## Testing

```bash
cargo fmt --all -- --check
cargo test --release
```

The suite covers full and partial downloads, malformed ranges, empty files, password unlock, revocation, expiration, download limits, folder ZIPs, host/origin protections, and transfer accounting.

## Release artifacts

Compiled executables and `cloudflared.exe` are intentionally excluded from the source repository. Package them separately and attach the finished ZIP to a GitHub Release. Include the required Cloudflare license and third-party notices in that release archive.

## Limitations

- The dashboard is browser-based, so selecting a file creates a local working copy.
- Quick Tunnel URLs are temporary and have no service-level guarantee.
- Cloudflare can process transfer contents in transit; this is not provider-blind end-to-end encryption.
- The sender's computer must remain awake, online, and running TempShare.
- The configured maximum file size is not a guarantee that every tunnel or network can transfer that size reliably.
- No native Android or iOS sender application is included.

## Screenshots

No screenshots are committed because the dashboard can display real filenames and temporary public URLs. Use sanitized images only if you add portfolio screenshots later.

## Copyright and use

Copyright (c) 2026 Muneer Mahmoud. All rights reserved.

This project is proprietary. Viewing the source on GitHub does not grant permission to use, copy, modify, distribute, sublicense, sell, or otherwise exploit it. See the [proprietary copyright notice](LICENSE). Third-party materials remain subject to their respective owners' rights and license terms.
