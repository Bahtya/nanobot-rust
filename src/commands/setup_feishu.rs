//! Feishu / Lark QR scan-to-create onboarding (device-code flow).
//!
//! Implements the OAuth 2.0 device-code registration flow:
//!   1. Init — verify `client_secret` auth is supported
//!   2. Begin — obtain `device_code`, `qr_url`, polling params
//!   3. Render QR — display scannable terminal QR code
//!   4. Poll — wait for user to scan (≤10 min)
//!   5. Probe — verify bot connectivity via `/open-apis/bot/v3/info`
//!
//! Callers use [`run_onboarding`] to get a [`RegistrationResult`]
//! and persist credentials themselves.

use anyhow::{bail, Context, Result};
use owo_colors::OwoColorize;
use qrcode::render::unicode;
use qrcode::QrCode;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::time::{Duration, Instant};

// ── Feishu / Lark endpoint constants ────────────────────────────

const ACCOUNTS_FEISHU: &str = "https://accounts.feishu.cn";
const ACCOUNTS_LARK: &str = "https://accounts.larksuite.com";
const OPEN_FEISHU: &str = "https://open.feishu.cn";
const OPEN_LARK: &str = "https://open.larksuite.com";
const REGISTRATION_PATH: &str = "/oauth/v1/app/registration";
const REQUEST_TIMEOUT_SECS: u64 = 10;
const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;
const DEFAULT_EXPIRE_SECS: u64 = 600;

fn accounts_base_url(domain: &str) -> &'static str {
    match domain {
        "lark" => ACCOUNTS_LARK,
        _ => ACCOUNTS_FEISHU,
    }
}

fn open_base_url(domain: &str) -> &'static str {
    match domain {
        "lark" => OPEN_LARK,
        _ => OPEN_FEISHU,
    }
}

// ── API response types ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct InitResponse {
    supported_auth_methods: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct BeginResponse {
    device_code: Option<String>,
    verification_uri_complete: Option<String>,
    user_code: Option<String>,
    interval: Option<u64>,
    expire_in: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct PollResponse {
    client_id: Option<String>,
    client_secret: Option<String>,
    error: Option<String>,
    user_info: Option<UserInfo>,
}

#[derive(Debug, Deserialize)]
struct UserInfo {
    open_id: Option<String>,
    tenant_brand: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    tenant_access_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BotInfoResponse {
    code: Option<i64>,
    bot: Option<BotData>,
    data: Option<BotDataWrapper>,
}

#[derive(Debug, Deserialize)]
struct BotData {
    app_name: Option<String>,
    bot_name: Option<String>,
    open_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BotDataWrapper {
    bot: Option<BotData>,
}

/// Successful registration result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrationResult {
    pub app_id: String,
    pub app_secret: String,
    pub domain: String,
    pub open_id: Option<String>,
    pub bot_name: Option<String>,
    pub bot_open_id: Option<String>,
}

// ── HTTP helpers ────────────────────────────────────────────────

fn build_http_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .build()
        .context("Failed to build HTTP client")
}

/// POST JSON from the registration endpoint.
///
/// The registration endpoint returns JSON even on 4xx (e.g. poll returns
/// `authorization_pending` as a 400). We always parse the body regardless
/// of HTTP status.
async fn post_registration(
    client: &Client,
    base_url: &str,
    body: &[(&str, &str)],
) -> Result<String> {
    let url = format!("{}{}", base_url, REGISTRATION_PATH);
    let resp = client
        .post(&url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(body)
        .send()
        .await
        .context("Failed to send registration request")?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .context("Failed to read registration response body")?;

    if !status.is_success() && text.is_empty() {
        bail!(
            "Registration request failed with HTTP {} (empty body)",
            status
        );
    }
    Ok(text)
}

/// Parse a JSON response, returning the typed value.
fn parse_json<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T> {
    serde_json::from_str(raw).with_context(|| {
        // Truncate long responses to avoid leaking sensitive data in logs
        let preview = if raw.len() > 200 {
            format!("{}...(truncated)", &raw[..200])
        } else {
            raw.to_string()
        };
        format!("Failed to parse JSON: {}", preview)
    })
}

// ── Step 1: Init ────────────────────────────────────────────────

async fn init_registration(client: &Client, domain: &str) -> Result<()> {
    let raw = post_registration(client, accounts_base_url(domain), &[("action", "init")]).await?;
    let resp: InitResponse = parse_json(&raw)?;
    let methods = resp.supported_auth_methods.unwrap_or_default();
    if !methods.iter().any(|m| m == "client_secret") {
        bail!(
            "Feishu / Lark registration environment does not support client_secret auth. Supported: {:?}",
            methods
        );
    }
    Ok(())
}

// ── Step 2: Begin ───────────────────────────────────────────────

struct BeginResult {
    device_code: String,
    qr_url: String,
    #[allow(dead_code)]
    user_code: String,
    interval: u64,
    expire_in: u64,
}

async fn begin_registration(client: &Client, domain: &str) -> Result<BeginResult> {
    let raw = post_registration(
        client,
        accounts_base_url(domain),
        &[
            ("action", "begin"),
            ("archetype", "PersonalAgent"),
            ("auth_method", "client_secret"),
            ("request_user_info", "open_id"),
        ],
    )
    .await?;
    let resp: BeginResponse = parse_json(&raw)?;
    let device_code = resp
        .device_code
        .ok_or_else(|| anyhow::anyhow!("Registration did not return a device_code"))?;
    let mut qr_url = resp.verification_uri_complete.unwrap_or_default();
    if !qr_url.is_empty() {
        let separator = if qr_url.contains('?') { '&' } else { '?' };
        qr_url = format!("{}{}from=kestrel&tp=kestrel", qr_url, separator);
    }
    Ok(BeginResult {
        device_code,
        qr_url,
        user_code: resp.user_code.unwrap_or_default(),
        interval: resp.interval.unwrap_or(DEFAULT_POLL_INTERVAL_SECS),
        expire_in: resp.expire_in.unwrap_or(DEFAULT_EXPIRE_SECS),
    })
}

// ── Step 3: Render QR ───────────────────────────────────────────

/// Render a QR code in the terminal using Unicode block characters.
/// Returns `true` if rendered successfully.
fn render_qr(url: &str) -> bool {
    match QrCode::new(url) {
        Ok(code) => {
            let image = code
                .render::<unicode::Dense1x2>()
                .dark_color(unicode::Dense1x2::Dark)
                .light_color(unicode::Dense1x2::Light)
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

// ── Step 4: Poll ────────────────────────────────────────────────

async fn poll_registration(
    client: &Client,
    device_code: &str,
    interval: u64,
    expire_in: u64,
    initial_domain: &str,
) -> Result<Option<RegistrationResult>> {
    let deadline = Instant::now() + Duration::from_secs(expire_in);
    let mut current_domain = initial_domain.to_string();
    let mut domain_switched = false;
    let mut poll_count: u64 = 0;
    let mut stdout = std::io::stdout();

    while Instant::now() < deadline {
        let raw = match post_registration(
            client,
            accounts_base_url(&current_domain),
            &[
                ("action", "poll"),
                ("device_code", device_code),
                ("tp", "ob_app"),
            ],
        )
        .await
        {
            Ok(r) => r,
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(interval)).await;
                continue;
            }
        };

        poll_count += 1;
        let resp: PollResponse = match parse_json(&raw) {
            Ok(r) => r,
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(interval)).await;
                continue;
            }
        };

        if poll_count == 1 {
            print!("  Fetching configuration results...");
        } else if poll_count.is_multiple_of(6) {
            print!(".");
        }
        let _ = stdout.flush();

        // Domain auto-detection
        if let Some(ref user_info) = resp.user_info {
            if user_info.tenant_brand.as_deref() == Some("lark") && !domain_switched {
                current_domain = "lark".to_string();
                domain_switched = true;
            }
        }

        // Success
        if let (Some(app_id), Some(app_secret)) = (resp.client_id, resp.client_secret) {
            if poll_count > 0 {
                println!();
            }
            let user_info = resp.user_info.unwrap_or(UserInfo {
                open_id: None,
                tenant_brand: None,
            });
            return Ok(Some(RegistrationResult {
                app_id,
                app_secret,
                domain: current_domain,
                open_id: user_info.open_id,
                bot_name: None,
                bot_open_id: None,
            }));
        }

        // Terminal errors
        if let Some(ref error) = resp.error {
            if error == "access_denied" || error == "expired_token" {
                if poll_count > 0 {
                    println!();
                }
                return Ok(None);
            }
        }

        tokio::time::sleep(Duration::from_secs(interval)).await;
    }

    if poll_count > 0 {
        println!();
    }
    Ok(None)
}

// ── Step 5: Probe bot ───────────────────────────────────────────

async fn probe_bot(
    client: &Client,
    app_id: &str,
    app_secret: &str,
    domain: &str,
) -> Option<(String, String)> {
    // Step A: get tenant_access_token
    let base = open_base_url(domain);
    let token_url = format!("{}/open-apis/auth/v3/tenant_access_token/internal", base);
    let token_body = serde_json::json!({
        "app_id": app_id,
        "app_secret": app_secret
    });

    let token_resp = client
        .post(&token_url)
        .header("Content-Type", "application/json")
        .json(&token_body)
        .send()
        .await
        .ok()?;

    let token_data: TokenResponse = token_resp.json().await.ok()?;
    let access_token = token_data.tenant_access_token?;

    // Step B: get bot info
    let bot_url = format!("{}/open-apis/bot/v3/info", base);
    let bot_resp = client
        .get(&bot_url)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Content-Type", "application/json")
        .send()
        .await
        .ok()?;

    let bot_data: BotInfoResponse = bot_resp.json().await.ok()?;
    if bot_data.code != Some(0) {
        return None;
    }

    // Try top-level bot first, then nested data.bot
    let bot = bot_data.bot.or_else(|| bot_data.data.and_then(|d| d.bot))?;
    let bot_name = bot.app_name.or(bot.bot_name).unwrap_or_default();
    let bot_open_id = bot.open_id.unwrap_or_default();

    if bot_name.is_empty() && bot_open_id.is_empty() {
        return None;
    }

    Some((bot_name, bot_open_id))
}

// ── Public entry point ─────────────────────────────────────────


/// Run the Feishu / Lark QR onboarding flow.
///
/// Prints QR code and progress to stdout. Returns the registration result
/// on success so the caller can persist credentials.
pub async fn run_onboarding(initial_domain: &str) -> Result<RegistrationResult> {
    let client = build_http_client()?;

    // Step 1: Init
    print!("  Connecting to Feishu / Lark...");
    std::io::stdout().flush().ok();
    init_registration(&client, initial_domain).await?;
    println!(" done.");

    // Step 2: Begin
    let begin = begin_registration(&client, initial_domain).await?;

    // Step 3: Render QR
    println!();
    println!("  Scan the QR code below with your Feishu or Lark mobile app:");
    if !render_qr(&begin.qr_url) {
        println!("  (QR rendering failed)");
    }
    println!("  Or open: {}\n", begin.qr_url.cyan().underline());

    // Step 4: Poll
    let result = poll_registration(
        &client,
        &begin.device_code,
        begin.interval,
        begin.expire_in,
        initial_domain,
    )
    .await
    .context("Polling failed")?;

    let mut result = match result {
        Some(r) => r,
        None => {
            println!();
            println!("  {} Registration timed out or was denied.", "!".yellow());
            println!("  {} Run `kestrel setup` again.", "!".yellow());
            bail!("Feishu / Lark registration timed out or was denied");
        }
    };

    // Step 5: Probe bot (best-effort)
    if let Some((bot_name, bot_open_id)) =
        probe_bot(&client, &result.app_id, &result.app_secret, &result.domain).await
    {
        result.bot_name = Some(bot_name);
        result.bot_open_id = Some(bot_open_id);
    }

    // Summary
    println!();
    println!("  {} Feishu app created automatically", "✓".green());
    if result.bot_name.is_some() || result.bot_open_id.is_some() {
        println!("  {} Bot connectivity verified", "✓".green());
    }
    println!("  Domain:    {}", result.domain.bold());
    println!("  App ID:    {}", result.app_id.dimmed());
    if let Some(ref name) = result.bot_name {
        println!("  Bot name:  {}", name.bold());
    }

    Ok(result)
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_accounts_base_url() {
        assert_eq!(accounts_base_url("feishu"), ACCOUNTS_FEISHU);
        assert_eq!(accounts_base_url("lark"), ACCOUNTS_LARK);
        assert_eq!(accounts_base_url("unknown"), ACCOUNTS_FEISHU);
    }

    #[test]
    fn test_open_base_url() {
        assert_eq!(open_base_url("feishu"), OPEN_FEISHU);
        assert_eq!(open_base_url("lark"), OPEN_LARK);
    }

    #[test]
    fn test_parse_init_response() {
        let raw = r#"{"supported_auth_methods":["client_secret","token"]}"#;
        let resp: InitResponse = parse_json(raw).unwrap();
        assert_eq!(
            resp.supported_auth_methods.unwrap(),
            vec!["client_secret", "token"]
        );
    }

    #[test]
    fn test_parse_init_response_empty_methods() {
        let raw = r#"{"supported_auth_methods":[]}"#;
        let resp: InitResponse = parse_json(raw).unwrap();
        assert!(resp.supported_auth_methods.unwrap().is_empty());
    }

    #[test]
    fn test_parse_begin_response() {
        let raw = r#"{"device_code":"dc_123","verification_uri_complete":"https://example.com/verify?code=abc","user_code":"ABC-DEF","interval":5,"expire_in":600}"#;
        let resp: BeginResponse = parse_json(raw).unwrap();
        assert_eq!(resp.device_code.unwrap(), "dc_123");
        assert_eq!(
            resp.verification_uri_complete.unwrap(),
            "https://example.com/verify?code=abc"
        );
        assert_eq!(resp.interval.unwrap(), 5);
        assert_eq!(resp.expire_in.unwrap(), 600);
    }

    #[test]
    fn test_parse_begin_response_defaults() {
        let raw = r#"{"device_code":"dc_456"}"#;
        let resp: BeginResponse = parse_json(raw).unwrap();
        assert_eq!(resp.device_code.unwrap(), "dc_456");
        assert!(resp.verification_uri_complete.is_none());
        assert!(resp.user_code.is_none());
        assert!(resp.interval.is_none());
        assert!(resp.expire_in.is_none());
    }

    #[test]
    fn test_parse_poll_response_success() {
        let raw = r#"{"client_id":"cli_123","client_secret":"secret_abc","user_info":{"open_id":"ou_123","tenant_brand":"feishu"}}"#;
        let resp: PollResponse = parse_json(raw).unwrap();
        assert_eq!(resp.client_id.unwrap(), "cli_123");
        assert_eq!(resp.client_secret.unwrap(), "secret_abc");
        let ui = resp.user_info.unwrap();
        assert_eq!(ui.open_id.unwrap(), "ou_123");
        assert_eq!(ui.tenant_brand.unwrap(), "feishu");
    }

    #[test]
    fn test_parse_poll_response_pending() {
        let raw = r#"{"error":"authorization_pending"}"#;
        let resp: PollResponse = parse_json(raw).unwrap();
        assert_eq!(resp.error.unwrap(), "authorization_pending");
        assert!(resp.client_id.is_none());
    }

    #[test]
    fn test_parse_poll_response_denied() {
        let raw = r#"{"error":"access_denied"}"#;
        let resp: PollResponse = parse_json(raw).unwrap();
        assert_eq!(resp.error.unwrap(), "access_denied");
    }

    #[test]
    fn test_parse_poll_response_expired() {
        let raw = r#"{"error":"expired_token"}"#;
        let resp: PollResponse = parse_json(raw).unwrap();
        assert_eq!(resp.error.unwrap(), "expired_token");
    }

    #[test]
    fn test_parse_bot_info_response() {
        let raw = r#"{"code":0,"bot":{"app_name":"MyBot","open_id":"ou_bot_123"}}"#;
        let resp: BotInfoResponse = parse_json(raw).unwrap();
        assert_eq!(resp.code.unwrap(), 0);
        let bot = resp.bot.unwrap();
        assert_eq!(bot.app_name.unwrap(), "MyBot");
        assert_eq!(bot.open_id.unwrap(), "ou_bot_123");
    }

    #[test]
    fn test_parse_bot_info_response_nested() {
        let raw = r#"{"code":0,"data":{"bot":{"bot_name":"LegacyBot","open_id":"ou_bot_456"}}}"#;
        let resp: BotInfoResponse = parse_json(raw).unwrap();
        assert!(resp.bot.is_none());
        let bot = resp.data.unwrap().bot.unwrap();
        assert_eq!(bot.bot_name.unwrap(), "LegacyBot");
        assert_eq!(bot.open_id.unwrap(), "ou_bot_456");
    }

    #[test]
    fn test_parse_bot_info_response_error() {
        let raw = r#"{"code":99999,"msg":"app not found"}"#;
        let resp: BotInfoResponse = parse_json(raw).unwrap();
        assert_eq!(resp.code.unwrap(), 99999);
        assert!(resp.bot.is_none());
    }

    #[test]
    fn test_parse_token_response() {
        let raw = r#"{"tenant_access_token":"tok_abc","expire":7200}"#;
        let resp: TokenResponse = parse_json(raw).unwrap();
        assert_eq!(resp.tenant_access_token.unwrap(), "tok_abc");
    }

    #[test]
    fn test_parse_token_response_failure() {
        let raw = r#"{"code":99999,"msg":"invalid app_id"}"#;
        let resp: TokenResponse = parse_json(raw).unwrap();
        assert!(resp.tenant_access_token.is_none());
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
    fn test_registration_result_serde_roundtrip() {
        let result = RegistrationResult {
            app_id: "cli_123".to_string(),
            app_secret: "secret".to_string(),
            domain: "feishu".to_string(),
            open_id: Some("ou_123".to_string()),
            bot_name: Some("TestBot".to_string()),
            bot_open_id: Some("ou_bot_456".to_string()),
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: RegistrationResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.app_id, result.app_id);
        assert_eq!(parsed.app_secret, result.app_secret);
        assert_eq!(parsed.domain, result.domain);
        assert_eq!(parsed.open_id, result.open_id);
        assert_eq!(parsed.bot_name, result.bot_name);
        assert_eq!(parsed.bot_open_id, result.bot_open_id);
    }

    #[test]
    fn test_qr_url_appends_params_with_question_mark() {
        let url = "https://example.com/verify";
        let separator = if url.contains('?') { '&' } else { '?' };
        let result = format!("{}{}from=kestrel&tp=kestrel", url, separator);
        assert_eq!(result, "https://example.com/verify?from=kestrel&tp=kestrel");
    }

    #[test]
    fn test_qr_url_appends_params_with_ampersand() {
        let url = "https://example.com/verify?code=abc";
        let separator = if url.contains('?') { '&' } else { '?' };
        let result = format!("{}{}from=kestrel&tp=kestrel", url, separator);
        assert_eq!(
            result,
            "https://example.com/verify?code=abc&from=kestrel&tp=kestrel"
        );
    }

    #[test]
    fn test_build_http_client() {
        let client = build_http_client();
        assert!(client.is_ok());
    }
}
