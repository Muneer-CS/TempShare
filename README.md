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

## Copyright and use

Copyright (c) 2026 Muneer Mahmoud. All rights reserved.

This project is proprietary. Viewing the source on GitHub does not grant permission to use, copy, modify, distribute, sublicense, sell, or otherwise exploit it. See the [proprietary copyright notice](LICENSE). Third-party materials remain subject to their respective owners' rights and license terms.
