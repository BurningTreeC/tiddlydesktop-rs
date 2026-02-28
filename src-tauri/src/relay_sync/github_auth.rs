//! Multi-provider OAuth Authorization Code flow for relay sync.
//!
//! Supports GitHub, GitLab, and OIDC providers. Flow:
//! 1. Open browser to provider's authorize URL with redirect_uri pointing to relay server
//! 2. User authorizes → provider redirects to relay server's callback endpoint
//! 3. Relay server exchanges code for token, stores result keyed by state token
//! 4. App retrieves result: Android via deep link notification, desktop via polling
//! 5. Return the token + user info

/// Timeout for the OAuth flow (user must authorize within this time)
const OAUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300); // 5 minutes

/// Polling interval for desktop auth result retrieval
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Generic auth result returned by all providers
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AuthResult {
    pub access_token: String,
    pub username: String,
    /// Prefixed user ID: "github:12345", "gitlab:67890", "oidc:sub"
    pub user_id: String,
    /// Provider name: "github", "gitlab", "oidc"
    pub provider: String,
}

/// Provider info returned by GET /api/auth/providers
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProviderInfo {
    pub name: String,
    #[serde(default)]
    pub client_id: String,
    /// Provider-specific base URL (e.g. GitLab instance URL)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// OIDC discovery URL
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_url: Option<String>,
    /// Human-readable name (e.g. "GitHub", "GitLab", "Company SSO")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// Fetch available auth providers from the relay server.
pub async fn fetch_providers(relay_url: &str) -> Result<Vec<ProviderInfo>, String> {
    let api_base = relay_ws_to_https(relay_url);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    let resp = client
        .get(format!("{}/api/auth/providers", api_base))
        .send()
        .await
        .map_err(|e| format!("Failed to fetch providers: {}", e))?;

    if !resp.status().is_success() {
        // Old servers don't have this endpoint — return GitHub as default
        return Ok(vec![ProviderInfo {
            name: "github".to_string(),
            client_id: String::new(),
            url: None,
            discovery_url: None,
            display_name: Some("GitHub".to_string()),
        }]);
    }

    let body: serde_json::Value = resp.json().await
        .map_err(|e| format!("Invalid providers response: {}", e))?;

    // Handle both {"providers": [...]} (wrapped) and [...] (bare array)
    let arr = if let Some(arr) = body.get("providers") {
        arr.clone()
    } else {
        body
    };

    let providers: Vec<ProviderInfo> = serde_json::from_value(arr)
        .map_err(|e| format!("Invalid providers list: {}", e))?;

    Ok(providers)
}

/// Start the OAuth Authorization Code flow using server-side callbacks.
///
/// Opens the browser to the provider's authorize URL with `redirect_uri` pointing
/// to the relay server's callback endpoint. Returns the state token used to
/// retrieve the auth result later.
///
/// - `provider`: "github", "gitlab", or "oidc"
/// - `client_id`: OAuth client ID from the relay server's provider config
/// - `auth_url`: The provider's authorization endpoint (for GitHub/GitLab)
/// - `discovery_url`: OIDC discovery URL (for OIDC providers — used to fetch auth endpoint)
/// - `scope`: OAuth scope string (defaults per-provider if None)
pub async fn start_auth_flow(
    relay_url: &str,
    provider: &str,
    client_id: &str,
    auth_url: Option<&str>,
    discovery_url: Option<&str>,
    scope: Option<&str>,
) -> Result<String, String> {
    if client_id.is_empty() {
        return Err(format!("OAuth not configured for {} (no client ID)", provider));
    }

    let api_base = relay_ws_to_https(relay_url);

    // Resolve the authorization URL
    let resolved_auth_url = resolve_auth_url(provider, auth_url, discovery_url).await?;

    // Resolve scope
    let resolved_scope = match scope {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => default_scope(provider).to_string(),
    };

    // Generate a random state token (32 bytes, hex-encoded = 64 chars)
    let state = generate_state_token()?;

    // redirect_uri points to the relay server's callback endpoint
    let redirect_uri = format!("{}/api/auth/callback/{}", api_base, provider);

    // Build authorization URL
    let full_auth_url = format!(
        "{}?client_id={}&redirect_uri={}&scope={}&response_type=code&state={}",
        resolved_auth_url,
        urlencoding::encode(client_id),
        urlencoding::encode(&redirect_uri),
        urlencoding::encode(&resolved_scope),
        urlencoding::encode(&state),
    );

    eprintln!("[Auth] Opening browser for {} OAuth, state={}...", provider, &state[..8]);

    open_browser(&full_auth_url)?;

    Ok(state)
}

/// Retrieve the auth result from the relay server by polling.
///
/// After the user authorizes in the browser, the relay server exchanges the code
/// and stores the result. This function polls until the result is available.
/// Used on desktop where there's no deep link to notify the app.
///
/// Transient network errors (connection refused, timeout, DNS failure) are retried
/// silently. Only persistent non-success HTTP responses are treated as fatal.
pub async fn poll_auth_result(
    relay_url: &str,
    state: &str,
) -> Result<AuthResult, String> {
    let api_base = relay_ws_to_https(relay_url);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    let url = format!("{}/api/auth/result?state={}", api_base, urlencoding::encode(state));

    let deadline = tokio::time::Instant::now() + OAUTH_TIMEOUT;
    let mut consecutive_errors: u32 = 0;

    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err("OAuth flow timed out (5 minutes). Please try again.".to_string());
        }

        let resp = match client.get(&url).send().await {
            Ok(r) => {
                consecutive_errors = 0;
                r
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors <= 3 {
                    eprintln!("[Auth] Poll transient error ({}/3), retrying: {}", consecutive_errors, e);
                } else if consecutive_errors == 4 {
                    eprintln!("[Auth] Poll repeated errors, continuing to retry silently...");
                }
                // Back off: 2s, 4s, 6s, ... capped at 10s
                let backoff = std::cmp::min(POLL_INTERVAL * consecutive_errors, std::time::Duration::from_secs(10));
                tokio::time::sleep(backoff).await;
                continue;
            }
        };

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            // Not ready yet — wait and retry
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Auth result retrieval failed ({}): {}", status, body));
        }

        let body: serde_json::Value = resp.json().await
            .map_err(|e| format!("Invalid auth result response: {}", e))?;

        let result = parse_auth_response(&body, "");
        eprintln!("[Auth] Auth complete: @{} via {}", result.username, result.provider);
        return Ok(result);
    }
}

/// Retrieve the auth result from the relay server (single attempt, no polling).
///
/// Used on Android after the deep link arrives — the result should already be
/// available on the server, so no polling is needed.
pub async fn fetch_auth_result(
    relay_url: &str,
    state: &str,
) -> Result<AuthResult, String> {
    let api_base = relay_ws_to_https(relay_url);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    let resp = client
        .get(format!("{}/api/auth/result?state={}", api_base, urlencoding::encode(state)))
        .send()
        .await
        .map_err(|e| format!("Failed to retrieve auth result: {}", e))?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err("Auth result not found or expired. Please try again.".to_string());
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Auth result retrieval failed ({}): {}", status, body));
    }

    let body: serde_json::Value = resp.json().await
        .map_err(|e| format!("Invalid auth result response: {}", e))?;

    let result = parse_auth_response(&body, "");
    eprintln!("[Auth] Deep link auth complete: @{} via {}", result.username, result.provider);
    Ok(result)
}

/// Resolve the authorization URL based on provider type.
async fn resolve_auth_url(
    provider: &str,
    auth_url: Option<&str>,
    discovery_url: Option<&str>,
) -> Result<String, String> {
    // If auth_url is explicitly provided, use it
    if let Some(url) = auth_url {
        if !url.is_empty() {
            return Ok(url.to_string());
        }
    }

    // For OIDC, fetch from discovery document
    if provider == "oidc" {
        if let Some(disc_url) = discovery_url {
            return fetch_oidc_authorization_endpoint(disc_url).await;
        }
        return Err("OIDC provider requires a discovery URL or authorization URL".to_string());
    }

    // Default auth URLs for well-known providers
    match provider {
        "github" => Ok("https://github.com/login/oauth/authorize".to_string()),
        "gitlab" => Ok("https://gitlab.com/oauth/authorize".to_string()),
        _ => Err(format!("No authorization URL configured for provider '{}'", provider)),
    }
}

/// Default OAuth scopes per provider
fn default_scope(provider: &str) -> &'static str {
    match provider {
        "github" => "read:user",
        "gitlab" => "read_user",
        "oidc" => "openid profile email",
        _ => "read:user",
    }
}

/// Generate a cryptographically random state token (32 bytes, hex-encoded).
fn generate_state_token() -> Result<String, String> {
    use rand::Rng;
    let bytes: [u8; 32] = rand::rng().random();
    Ok(bytes.iter().map(|b| format!("{:02x}", b)).collect::<String>())
}

/// Fetch the authorization_endpoint from an OIDC discovery document.
async fn fetch_oidc_authorization_endpoint(discovery_url: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    let resp = client
        .get(discovery_url)
        .send()
        .await
        .map_err(|e| format!("Failed to fetch OIDC discovery document: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("OIDC discovery failed ({})", resp.status()));
    }

    let doc: serde_json::Value = resp.json().await
        .map_err(|e| format!("Invalid OIDC discovery document: {}", e))?;

    doc["authorization_endpoint"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "OIDC discovery document missing authorization_endpoint".to_string())
}

/// Parse auth response from relay server, handling both new and legacy formats.
fn parse_auth_response(body: &serde_json::Value, provider: &str) -> AuthResult {
    // Try new generic fields first
    let username = body["username"].as_str()
        .or_else(|| body["github_login"].as_str())  // legacy fallback
        .unwrap_or("")
        .to_string();

    let user_id = body["user_id"].as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            // Legacy fallback: construct from github_id
            if let Some(id) = body["github_id"].as_i64() {
                format!("github:{}", id)
            } else {
                String::new()
            }
        });

    let provider_from_server = body["provider"].as_str()
        .unwrap_or(provider)
        .to_string();

    let access_token = body["access_token"].as_str()
        .unwrap_or("")
        .to_string();

    AuthResult {
        access_token,
        username,
        user_id,
        provider: provider_from_server,
    }
}

/// Convert relay WebSocket URL (wss://host:port) to HTTPS API URL (https://host:port)
pub fn relay_ws_to_https(relay_url: &str) -> String {
    relay_url
        .replace("wss://", "https://")
        .replace("ws://", "http://")
}

/// Open a URL in the system default browser
fn open_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map_err(|e| format!("Failed to open browser: {}", e))?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map_err(|e| format!("Failed to open browser: {}", e))?;
    }
    #[cfg(target_os = "windows")]
    {
        // Use raw_arg to pass the URL in double-quotes, preventing cmd.exe
        // from interpreting & in query parameters as command separators.
        // Without quoting, "start "" https://...?a=1&b=2" truncates at the first &.
        use std::os::windows::process::CommandExt;
        std::process::Command::new("cmd")
            .raw_arg(format!("/c start \"\" \"{}\"", url))
            .spawn()
            .map_err(|e| format!("Failed to open browser: {}", e))?;
    }
    #[cfg(target_os = "android")]
    {
        // Use tauri-plugin-opener via the global app handle
        use tauri_plugin_opener::OpenerExt;
        let app = crate::get_global_app_handle()
            .ok_or("App handle not available")?;
        app.opener()
            .open_url(url, None::<&str>)
            .map_err(|e| format!("Failed to open browser: {}", e))?;
    }
    Ok(())
}

/// Validate a stored auth token by calling the relay server's /api/auth/user endpoint.
/// Returns the user info if valid, or an error if expired/invalid.
pub async fn validate_token(relay_url: &str, token: &str, provider: &str) -> Result<AuthResult, String> {
    let api_base = relay_ws_to_https(relay_url);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    let resp = client
        .get(format!("{}/api/auth/user", api_base))
        .header("Authorization", format!("Bearer {}", token))
        .header("X-Auth-Provider", provider)
        .send()
        .await
        .map_err(|e| format!("Token validation request failed: {}", e))?;

    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("Token expired or invalid".to_string());
    }
    if !resp.status().is_success() {
        return Err(format!("Validation failed: {}", resp.status()));
    }

    let body: serde_json::Value = resp.json().await
        .map_err(|e| format!("Invalid response: {}", e))?;

    let mut result = parse_auth_response(&body, provider);
    result.access_token = token.to_string();
    Ok(result)
}
