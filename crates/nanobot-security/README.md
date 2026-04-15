# nanobot-security

SSRF protection, private IP blocking, and URL validation.

Part of the [nanobot-rust](../..) workspace.

## Overview

Prevents the agent from making outbound HTTP requests to internal/private network
addresses. Critical when the agent executes web fetch or search tools -- it must not
reach localhost, cloud metadata endpoints, or other internal services.

## Key Types

| Type | Description |
|---|---|
| `SsrfGuard` | Core SSRF checker with blocked-networks list and optional whitelist |

## SsrfGuard API

- `new()` -- Create with default blocked networks (RFC 1918, loopback, link-local, etc.)
- `is_ip_allowed(&ip)` -- Check whether an IP address passes the filter
- `validate_url(url_str)` -- Parse a URL, resolve its host, and check the IP
- `add_whitelist(cidr)` / `add_whitelists(&[cidr])` -- Exempt CIDR ranges from blocking
- `contains_internal_urls(text)` -- Heuristic scan for internal URL patterns in text

## Blocked Networks

`0.0.0.0/8`, `10.0.0.0/8`, `127.0.0.0/8`, `169.254.0.0/16`, `172.16.0.0/12`,
`192.168.0.0/16`, documentation ranges, multicast, IPv6 loopback/link-local/unique-local,
and cloud metadata IP `169.254.169.254/32`.

## Usage

```rust
use nanobot_security::SsrfGuard;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
let guard = SsrfGuard::new();

guard.validate_url("https://example.com/api").await?;   // OK
guard.validate_url("http://127.0.0.1:8080").await?;     // Error: SSRF blocked

// Whitelist specific ranges (e.g. Tailscale)
let mut guard = SsrfGuard::new();
guard.add_whitelist("100.64.0.0/10")?;
assert!(guard.is_ip_allowed(&"100.100.100.100".parse()?));
Ok(())
}
```
