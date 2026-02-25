//! Multi-provider OAuth Authorization Code flow for relay sync.
//!
//! Supports GitHub, GitLab, and OIDC providers. Flow:
//! 1. Bind a temporary HTTP server on localhost (random port)
//! 2. Open browser to provider's authorize URL with redirect_uri = http://localhost:{port}/callback
//! 3. User authorizes → provider redirects to localhost with ?code=...
//! 4. Capture the code, serve a "success" HTML page
//! 5. POST /api/auth/exchange/{provider} on the relay server to exchange code for access_token
//! 6. Return the token + user info

/// Timeout for the OAuth flow (user must authorize within this time)
const OAUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300); // 5 minutes

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

/// Start the OAuth Authorization Code flow for a given provider.
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
) -> Result<AuthResult, String> {
    if client_id.is_empty() {
        return Err(format!("OAuth not configured for {} (no client ID)", provider));
    }

    // Convert wss:// relay URL to https:// for REST API calls
    let api_base = relay_ws_to_https(relay_url);

    // Resolve the authorization URL
    let resolved_auth_url = resolve_auth_url(provider, auth_url, discovery_url).await?;

    // Resolve scope
    let resolved_scope = match scope {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => default_scope(provider).to_string(),
    };

    // Step 1: Bind temporary HTTP server on random port
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("Failed to bind OAuth callback server: {}", e))?;

    let local_addr = listener.local_addr()
        .map_err(|e| format!("Failed to get local address: {}", e))?;
    let port = local_addr.port();
    let redirect_uri = format!("http://localhost:{}/callback", port);

    eprintln!("[Auth] OAuth callback server on port {} for {}", port, provider);

    // Step 2: Build authorization URL and open browser
    let full_auth_url = format!(
        "{}?client_id={}&redirect_uri={}&scope={}&response_type=code",
        resolved_auth_url,
        urlencoding::encode(client_id),
        urlencoding::encode(&redirect_uri),
        urlencoding::encode(&resolved_scope),
    );

    // Open browser using platform-specific method
    open_browser(&full_auth_url)?;

    // Step 3: Wait for the callback (with timeout)
    let code = tokio::time::timeout(OAUTH_TIMEOUT, wait_for_callback(listener))
        .await
        .map_err(|_| "OAuth flow timed out (5 minutes). Please try again.".to_string())?
        .map_err(|e| format!("OAuth callback failed: {}", e))?;

    eprintln!("[Auth] Received authorization code for {}", provider);

    // Step 4: Exchange code for token via relay server
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    let resp = client
        .post(format!("{}/api/auth/exchange/{}", api_base, provider))
        .json(&serde_json::json!({
            "code": code,
            "redirect_uri": redirect_uri,
        }))
        .send()
        .await
        .map_err(|e| format!("Token exchange request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Token exchange failed ({}): {}", status, body));
    }

    let body: serde_json::Value = resp.json().await
        .map_err(|e| format!("Invalid token exchange response: {}", e))?;

    let result = parse_auth_response(&body, provider);
    eprintln!("[Auth] Authenticated as @{} via {}", result.username, result.provider);
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

/// Wait for the OAuth callback on the temporary HTTP server.
/// Returns the authorization code from the ?code= query parameter.
async fn wait_for_callback(listener: tokio::net::TcpListener) -> Result<String, String> {
    // Accept one connection
    let (stream, _addr) = listener.accept()
        .await
        .map_err(|e| format!("Accept failed: {}", e))?;

    // Read the HTTP request
    let mut buf = vec![0u8; 4096];
    stream.readable().await.map_err(|e| format!("Readable wait failed: {}", e))?;
    let n = stream.try_read(&mut buf).map_err(|e| format!("Read failed: {}", e))?;
    let request = String::from_utf8_lossy(&buf[..n]);

    // Parse the request line to extract the path + query
    let first_line = request.lines().next().unwrap_or("");
    let path = first_line.split_whitespace().nth(1).unwrap_or("");

    // Extract ?code= parameter
    let code = extract_query_param(path, "code");

    // Send response HTML
    let (status_line, body) = if let Some(ref code) = code {
        if code.is_empty() {
            ("400 Bad Request", "<html><body><h1>Error</h1><p>No authorization code received.</p></body></html>")
        } else {
            ("200 OK", "<html><body style=\"font-family:system-ui;text-align:center;padding:60px\"><h1>Authentication Successful!</h1><p>You can close this tab and return to TiddlyDesktop.</p></body></html>")
        }
    } else {
        ("400 Bad Request", "<html><body><h1>Authentication Failed</h1><p>The provider did not return an authorization code. Please try again.</p></body></html>")
    };

    let response = format!(
        "HTTP/1.1 {}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status_line,
        body.len(),
        body
    );

    stream.writable().await.map_err(|e| format!("Writable wait failed: {}", e))?;
    let _ = stream.try_write(response.as_bytes());

    // Small delay to ensure response is sent before closing
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    code.ok_or_else(|| "No authorization code in callback".to_string())
}

/// Extract a query parameter from a URL path.
fn extract_query_param(path: &str, param: &str) -> Option<String> {
    let query = path.split('?').nth(1)?;
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        if let (Some(key), Some(value)) = (parts.next(), parts.next()) {
            if key == param {
                return Some(urlencoding::decode(value).unwrap_or_default().into_owned());
            }
        }
    }
    None
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
        std::process::Command::new("cmd")
            .args(["/c", "start", "", url])
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
