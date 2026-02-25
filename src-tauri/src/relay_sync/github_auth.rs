//! GitHub OAuth Authorization Code flow for relay sync.
//!
//! Flow:
//! 1. Bind a temporary HTTP server on localhost (random port)
//! 2. Open browser to GitHub authorize URL with redirect_uri = http://localhost:{port}/callback
//! 3. User authorizes → GitHub redirects to localhost with ?code=...
//! 4. Capture the code, serve a "success" HTML page
//! 5. POST /api/auth/exchange on the relay server to exchange code for access_token
//! 6. Return the token + user info

/// GitHub OAuth App client ID (public — safe to embed in client code).
/// This must be filled in after creating the OAuth App at https://github.com/settings/developers
pub const GITHUB_CLIENT_ID: &str = ""; // TODO: fill in after creating GitHub OAuth App

const GITHUB_AUTH_URL: &str = "https://github.com/login/oauth/authorize";

/// Timeout for the OAuth flow (user must authorize within this time)
const OAUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300); // 5 minutes

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OAuthResult {
    pub access_token: String,
    pub github_login: String,
    pub github_id: i64,
}

/// Start the OAuth Authorization Code flow.
///
/// 1. Binds a temporary TCP listener on 127.0.0.1:0 (random port)
/// 2. Opens the browser to GitHub's authorization page
/// 3. Waits for the callback with the authorization code
/// 4. Exchanges the code via the relay server's /api/auth/exchange endpoint
/// 5. Returns the access token and user info
pub async fn start_auth_flow(relay_url: &str) -> Result<OAuthResult, String> {
    if GITHUB_CLIENT_ID.is_empty() {
        return Err("GitHub OAuth not configured (no client ID)".to_string());
    }

    // Convert wss:// relay URL to https:// for REST API calls
    let api_base = relay_ws_to_https(relay_url);

    // Step 1: Bind temporary HTTP server on random port
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("Failed to bind OAuth callback server: {}", e))?;

    let local_addr = listener.local_addr()
        .map_err(|e| format!("Failed to get local address: {}", e))?;
    let port = local_addr.port();
    let redirect_uri = format!("http://localhost:{}/callback", port);

    eprintln!("[GitHub Auth] OAuth callback server on port {}", port);

    // Step 2: Build authorization URL and open browser
    let auth_url = format!(
        "{}?client_id={}&redirect_uri={}&scope=read:user",
        GITHUB_AUTH_URL,
        GITHUB_CLIENT_ID,
        urlencoding::encode(&redirect_uri),
    );

    // Open browser using the opener crate or platform-specific method
    open_browser(&auth_url)?;

    // Step 3: Wait for the callback (with timeout)
    let code = tokio::time::timeout(OAUTH_TIMEOUT, wait_for_callback(listener))
        .await
        .map_err(|_| "OAuth flow timed out (5 minutes). Please try again.".to_string())?
        .map_err(|e| format!("OAuth callback failed: {}", e))?;

    eprintln!("[GitHub Auth] Received authorization code");

    // Step 4: Exchange code for token via relay server
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    let resp = client
        .post(format!("{}/api/auth/exchange", api_base))
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

    let result: OAuthResult = resp.json().await
        .map_err(|e| format!("Invalid token exchange response: {}", e))?;

    eprintln!("[GitHub Auth] Authenticated as @{}", result.github_login);
    Ok(result)
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
        // Check for error parameter
        ("400 Bad Request", "<html><body><h1>Authentication Failed</h1><p>GitHub did not return an authorization code. Please try again.</p></body></html>")
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
        // On Android, use Tauri's shell open or JNI
        // For now, this is handled at the Tauri command level
        let _ = url;
    }
    Ok(())
}

/// Validate a stored GitHub token by calling the relay server's /api/auth/user endpoint.
/// Returns the user info if valid, or an error if expired/invalid.
pub async fn validate_token(relay_url: &str, token: &str) -> Result<OAuthResult, String> {
    let api_base = relay_ws_to_https(relay_url);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    let resp = client
        .get(format!("{}/api/auth/user", api_base))
        .header("Authorization", format!("Bearer {}", token))
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

    Ok(OAuthResult {
        access_token: token.to_string(),
        github_login: body["github_login"].as_str().unwrap_or("").to_string(),
        github_id: body["github_id"].as_i64().unwrap_or(0),
    })
}
