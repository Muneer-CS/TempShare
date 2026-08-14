# Security policy

## Supported version

This portfolio repository currently tracks one development line. Security fixes should be applied to the latest revision of the default branch.

## Reporting a vulnerability

Do not publish exploit details, credentials, private links, or sample user files in a public issue. Use GitHub's private vulnerability reporting feature if it is enabled for the repository. Otherwise, contact the repository owner privately and provide:

- the affected revision or release;
- the operating system and configuration;
- a concise description of the impact;
- safe reproduction details that do not expose third-party data; and
- any suggested remediation.

## Deployment guidance

- Keep the management listener on loopback (`127.0.0.1:7420`).
- Tunnel or reverse-proxy only the public download listener.
- Use HTTPS for any recipient traffic that leaves the local computer.
- Use short expirations, low download limits, and passwords for sensitive shares.
- Revoke shares and delete local working copies after transfer.
- Never commit `.env`, `data`, database files, uploaded content, logs, or tunnel credentials.
- Treat Quick Tunnel URLs as bearer-like temporary links and do not publish them.
- Keep Rust dependencies, TempShare, and `cloudflared` updated.

## Privacy boundary

The optional Cloudflare tunnel terminates HTTPS at Cloudflare. TempShare does not permanently upload files to cloud storage, but traffic is not end-to-end encrypted against the tunnel provider.

