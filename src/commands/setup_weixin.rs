//! WeChat iLink Bot API QR scan onboarding.
//!
//! Implements the QR login flow:
//!   1. Fetch QR code from iLink `get_bot_qrcode`
//!   2. Render QR in terminal using Unicode block characters
//!   3. Poll `get_qrcode_status` until scan confirmed (≤8 min)
//!   4. Persist credentials to `config.toml`

use anyhow::{bail, Context, Result};
use kestrel_config::{
    loader::{load_config, save_config},
    paths::get_config_path,
    schema::{Config, WeixinConfig},
};
use owo_colors::OwoColorize;
use reqwest::Client;
use serde::Deserialize;
use std::io::Write;
use std::time::{Duration, Instant};

// ── iLink endpoint constants ────────────────────────────────────

const ILINK_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
const EP_GET_BOT_QR: &str = "ilink/bot/get_bot_qrcode";
const EP_GET_QR_STATUS: &str = "ilink/bot/get_qrcode_status";
const QR_TIMEOUT_MS: u64 = 35_000;
const POLL_INTERVAL_SECS: u64 = 1;
const TOTAL_TIMEOUT_SECS: u64 = 480;
const ILINK_APP_ID: &str = "bot";
const ILINK_APP_CLIENT_VERSION: u32 = (2 << 16) | (2 << 8);

// ── API response types ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct QrResponse {
    qrcode: Option<String>,
    qrcode_img_content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StatusResponse {
    status: Option<String>,
    redirect_host: Option<String>,
    ilink_bot_id: Option<String>,
    bot_token: Option<String>,
    baseurl: Option<String>,
    ilink_user_id: Option<String>,
}

// ── HTTP helper ─────────────────────────────────────────────────

fn build_http_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_millis(QR_TIMEOUT_MS))
        .build()
        .context("Failed to build HTTP client")
}

async fn api_get<T: serde::de::DeserializeOwned>(
    client: &Client,
    base_url: &str,
    endpoint: &str,
) -> Result<T> {
    let url = format!("{}/{}", base_url.trim_end_matches('/'), endpoint);
    let resp = client
        .get(&url)
        .header("iLink-App-Id", ILINK_APP_ID)
        .header("iLink-App-ClientVersion", ILINK_APP_CLIENT_VERSION.to_string())
        .send()
        .await
        .with_context(|| format!("GET {}", endpoint))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .with_context(|| format!("Failed to read body for {}", endpoint))?;

    if !status.is_success() {
        bail!("iLink GET {} HTTP {}: {}", endpoint, status, &text[..text.len().min(200)]);
    }

    serde_json::from_str(&text).with_context(|| {
        let preview = if text.len() > 200 {
            format!("{}...(truncated)", &text[..200])
        } else {
            text.clone()
        };
        format!("Failed to parse JSON: {}", preview)
    })
}

// ── Step 1: Fetch QR code ───────────────────────────────────────

async fn fetch_qr(client: &Client, base_url: &str) -> Result<(String, String)> {
    let resp: QrResponse =
        api_get(client, base_url, &format!("{}?bot_type=3", EP_GET_BOT_QR)).await?;

    let qrcode_value = resp.qrcode.unwrap_or_default();
    let qrcode_url = resp.qrcode_img_content.unwrap_or_default();

    if qrcode_value.is_empty() {
        bail!("QR response missing qrcode token");
    }

    // WeChat needs to scan the full liteapp URL, not the raw hex string
    let qr_scan_data = if !qrcode_url.is_empty() {
        qrcode_url.clone()
    } else {
        qrcode_value.clone()
    };

    Ok((qrcode_value, qr_scan_data))
}

// ── Step 2: Render QR ───────────────────────────────────────────

fn render_qr(url: &str) -> bool {
    match qrcode::QrCode::new(url) {
        Ok(code) => {
            let image = code
                .render::<qrcode::render::unicode::Dense1x2>()
                .dark_color(qrcode::render::unicode::Dense1x2::Dark)
                .light_color(qrcode::render::unicode::Dense1x2::Light)
                .build();
            println!();
            for line in image.lines() {
                println!("  {}", line);
            }
            println!();
            true
        }
        Err(_) => false,
    }
}

// ── Step 3: Poll status ─────────────────────────────────────────

async fn poll_status(
    client: &Client,
    initial_base_url: &str,
    qrcode_value: &str,
) -> Result<Option<Credentials>> {
    let deadline = Instant::now() + Duration::from_secs(TOTAL_TIMEOUT_SECS);
    let mut current_base_url = initial_base_url.to_string();
    let mut refresh_count: u32 = 0;
    let mut qrcode_token = qrcode_value.to_string();
    let mut stdout = std::io::stdout();
    let mut scaned_printed = false;

    while Instant::now() < deadline {
        let resp: StatusResponse = match api_get(
            client,
            &current_base_url,
            &format!("{}?qrcode={}", EP_GET_QR_STATUS, qrcode_token),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
                continue;
            }
        };

        let status = resp.status.unwrap_or_default();
        match status.as_str() {
            "wait" => {
                print!(".");
                let _ = stdout.flush();
            }
            "scaned" => {
                if !scaned_printed {
                    println!();
                    println!("  已扫码，请在微信里确认...");
                    scaned_printed = true;
                }
            }
            "scaned_but_redirect" => {
                if let Some(ref host) = resp.redirect_host {
                    current_base_url = format!("https://{}", host);
                }
            }
            "expired" => {
                refresh_count += 1;
                if refresh_count > 3 {
                    println!("\n  二维码多次过期，请重新执行登录。");
                    return Ok(None);
                }
                println!("\n  二维码已过期，正在刷新... ({}/3)", refresh_count);
                match fetch_qr(client, ILINK_BASE_URL).await {
                    Ok((new_token, new_scan_data)) => {
                        qrcode_token = new_token;
                        if !new_scan_data.is_empty() {
                            println!("  {}", new_scan_data.cyan().underline());
                        }
                        if !render_qr(&new_scan_data) {
                            println!("  (QR rendering failed)");
                        }
                    }
                    Err(e) => {
                        println!("  {} QR refresh failed: {}", "!".yellow(), e);
                        return Ok(None);
                    }
                }
            }
            "confirmed" => {
                let account_id = resp.ilink_bot_id.unwrap_or_default();
                let token = resp.bot_token.unwrap_or_default();
                let base_url = resp.baseurl.unwrap_or_else(|| ILINK_BASE_URL.to_string());
                let _user_id = resp.ilink_user_id.unwrap_or_default();

                if account_id.is_empty() || token.is_empty() {
                    bail!("QR confirmed but credential payload was incomplete");
                }

                println!("\n  微信连接成功，account_id={}", account_id.dimmed());
                return Ok(Some(Credentials {
                    account_id,
                    token,
                    base_url,
                }));
            }
            _ => {
                // Unknown status, keep polling
            }
        }

        tokio::time::sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
    }

    println!("\n  微信登录超时。");
    Ok(None)
}

// ── Credential result ───────────────────────────────────────────

#[derive(Debug, Clone)]
struct Credentials {
    account_id: String,
    token: String,
    base_url: String,
}

// ── Persist credentials ─────────────────────────────────────────

fn persist_credentials(creds: &Credentials) -> Result<()> {
    let config_path = get_config_path()?;

    let mut config = if config_path.exists() {
        load_config(Some(&config_path))?
    } else {
        Config::default()
    };

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create config directory: {}", parent.display()))?;
    }

    // Preserve existing weixin fields not set by QR login
    let (
        app_id,
        app_secret,
        old_token,
        encoding_aes_key,
        cdn_base_url,
        dm_policy,
        group_policy,
        allowed_users,
        group_allowed_users,
    ) = config
        .channels
        .weixin
        .as_ref()
        .map(|w| {
            (
                w.app_id.clone(),
                w.app_secret.clone(),
                w.token.clone(),
                w.encoding_aes_key.clone(),
                w.cdn_base_url.clone(),
                w.dm_policy.clone(),
                w.group_policy.clone(),
                w.allowed_users.clone(),
                w.group_allowed_users.clone(),
            )
        })
        .unwrap_or_default();

    config.channels.weixin = Some(WeixinConfig {
        account_id: Some(creds.account_id.clone()),
        bot_token: Some(creds.token.clone()),
        base_url: Some(creds.base_url.clone()),
        app_id,
        app_secret,
        token: old_token,
        encoding_aes_key,
        cdn_base_url,
        dm_policy,
        group_policy,
        allowed_users,
        group_allowed_users,
        enabled: true,
    });

    save_config(&config, &config_path)?;
    Ok(())
}

// ── Main entry point ────────────────────────────────────────────

/// Run the WeChat iLink QR scan onboarding flow.
pub fn run() -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("Failed to create tokio runtime")?
        .block_on(run_inner())
}

async fn run_inner() -> Result<()> {
    println!();
    println!("  {} {}", "▸".cyan(), "WeChat iLink Setup".bold().cyan());
    println!("  {}", "─".repeat(45).dimmed());
    println!();

    let client = build_http_client()?;

    // Step 1: Fetch QR
    print!("  Fetching QR code from iLink...");
    std::io::stdout().flush().ok();
    let (qrcode_value, qr_scan_data) = fetch_qr(&client, ILINK_BASE_URL).await?;
    println!(" done.");

    // Step 2: Render QR
    println!();
    println!("  请使用微信扫描以下二维码：");
    if !qr_scan_data.is_empty() {
        println!("  {}", qr_scan_data.cyan().underline());
    }
    if !render_qr(&qr_scan_data) {
        println!("  (终端二维码渲染失败，请直接打开上面的二维码链接)");
    }

    // Step 3: Poll
    println!("  等待扫码...");
    let creds = match poll_status(&client, ILINK_BASE_URL, &qrcode_value).await? {
        Some(c) => c,
        None => {
            println!();
            println!("  {} Registration timed out or was denied.", "!".yellow());
            println!(
                "  {} Please run `kestrel setup weixin` again.",
                "!".yellow()
            );
            bail!("WeChat iLink registration timed out or was denied");
        }
    };

    // Step 4: Persist
    persist_credentials(&creds)?;

    // Summary
    println!();
    println!("  {} WeChat iLink connected", "✓".green());
    println!("  {} Credentials saved to config.toml", "✓".green());
    println!();
    println!("  Account ID: {}", creds.account_id.bold());
    println!();
    println!("  {} WeChat setup complete!", "✓".green());

    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_http_client() {
        let client = build_http_client();
        assert!(client.is_ok());
    }

    #[test]
    fn test_render_qr_valid_url() {
        assert!(render_qr("https://example.com/test"));
    }

    #[test]
    fn test_render_qr_empty_url() {
        assert!(render_qr(""));
    }

    #[test]
    fn test_qr_response_parsing() {
        let raw = r#"{"qrcode":"abc123","qrcode_img_content":"https://example.com/qr"}"#;
        let resp: QrResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.qrcode.unwrap(), "abc123");
        assert_eq!(resp.qrcode_img_content.unwrap(), "https://example.com/qr");
    }

    #[test]
    fn test_status_response_parsing() {
        let raw = serde_json::json!({
            "status": "confirmed",
            "ilink_bot_id": "wxid_123",
            "bot_token": "tok_abc",
            "baseurl": "https://ilinkai.weixin.qq.com"
        })
        .to_string();
        let resp: StatusResponse = serde_json::from_str(&raw).unwrap();
        assert_eq!(resp.status.unwrap(), "confirmed");
        assert_eq!(resp.ilink_bot_id.unwrap(), "wxid_123");
        assert_eq!(resp.bot_token.unwrap(), "tok_abc");
        assert_eq!(resp.baseurl.unwrap(), "https://ilinkai.weixin.qq.com");
    }

    #[test]
    fn test_status_response_redirect() {
        let raw = r#"{"status":"scaned_but_redirect","redirect_host":"ilinkai.weixin.qq.com"}"#;
        let resp: StatusResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.status.unwrap(), "scaned_but_redirect");
        assert_eq!(resp.redirect_host.unwrap(), "ilinkai.weixin.qq.com");
    }

    #[test]
    fn test_persist_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");

        // Override config path for testing
        std::env::set_var("KESTREL_HOME", tmp.path().to_str().unwrap());

        let creds = Credentials {
            account_id: "wxid_test".to_string(),
            token: "tok_test".to_string(),
            base_url: "https://ilinkai.weixin.qq.com".to_string(),
        };

        persist_credentials(&creds).unwrap();

        assert!(config_path.exists());
        let loaded = load_config(Some(&config_path)).unwrap();
        let weixin = loaded.channels.weixin.unwrap();
        assert_eq!(weixin.account_id.unwrap(), "wxid_test");
        assert_eq!(weixin.bot_token.unwrap(), "tok_test");
        assert_eq!(weixin.base_url.unwrap(), "https://ilinkai.weixin.qq.com");
        assert!(weixin.enabled);

        std::env::remove_var("KESTREL_HOME");
    }
}
