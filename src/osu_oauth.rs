//! osu! OAuth login proxied through the Cloudflare Worker.
//!
//! The official osu! beatmapset download endpoint requires a per-user OAuth
//! token (`authorization_code` grant). Client credentials alone are not enough,
//! so the desktop app authenticates the user through the Worker:
//!
//! 1. The app opens `{backend}/oauth/authorize?...` in the user's browser.
//! 2. osu! redirects back to `http://127.0.0.1:3000/callback?code=...&state=...`.
//! 3. A loopback listener in this module captures the `code`.
//! 4. The code is exchanged via `POST {backend}/oauth/token` (the Worker holds
//!    the client secret, so it never ships inside the EXE).
//! 5. The refresh token is persisted under `.osu-map-manager/oauth.json` and
//!    refreshed via `POST {backend}/oauth/refresh` whenever it is expiring.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{BufRead, Write},
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const OAUTH_REDIRECT_URI: &str = "http://127.0.0.1:3000/callback";
/// Built-in backend Worker URL. Kept out of the UI so it is not exposed or
/// edited by users. Override with `OSU_MAP_MANAGER_BACKEND_URL` for local dev.
pub const BACKEND_URL: &str = "https://osu-map-manager.stanislavberman.workers.dev";
const OAUTH_CALLBACK_TIMEOUT: Duration = Duration::from_secs(180);
const REFRESH_MARGIN_SECS: u64 = 120;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OauthSession {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix timestamp (seconds) at which the access token expires.
    pub expires_at_unix: u64,
}

impl OauthSession {
    pub fn needs_refresh(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        self.access_token.trim().is_empty()
            || self.expires_at_unix.saturating_sub(REFRESH_MARGIN_SECS) <= now
    }
}

#[derive(Debug, Clone, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_in: u64,
}

impl TokenResponse {
    fn into_session(self) -> Result<OauthSession> {
        if self.access_token.trim().is_empty() {
            anyhow::bail!("OAuth token response did not contain an access token");
        }
        if self.refresh_token.trim().is_empty() {
            anyhow::bail!("OAuth token response did not contain a refresh token");
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        Ok(OauthSession {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at_unix: now + self.expires_in.max(60),
        })
    }
}

pub fn backend_base_url(backend_url: &str) -> String {
    backend_url.trim().trim_end_matches('/').to_owned()
}

/// Returns the effective backend URL: `OSU_MAP_MANAGER_BACKEND_URL` env
/// override when set, otherwise the built-in [`BACKEND_URL`].
pub fn backend_url() -> String {
    let from_env = std::env::var("OSU_MAP_MANAGER_BACKEND_URL")
        .map(|value| value.trim().trim_end_matches('/').to_owned())
        .ok()
        .filter(|value| !value.is_empty());
    from_env.unwrap_or_else(|| BACKEND_URL.to_owned())
}

pub fn authorize_url(backend_url: &str, state: &str, code_challenge: &str) -> String {
    format!(
        "{}/oauth/authorize?redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
        backend_base_url(backend_url),
        url_encode(OAUTH_REDIRECT_URI),
        url_encode(state),
        url_encode(code_challenge),
    )
}

/// Unpredictable OAuth `state` (CSRF protection) from the OS RNG.
pub fn generate_state() -> String {
    random_alphanumeric(24)
}

/// PKCE code verifier (RFC 7636 §4.1): 64 unreserved characters from the OS
/// RNG. Only the verifier's SHA-256 challenge passes through the browser;
/// the verifier itself travels over HTTPS to the Worker.
pub fn generate_code_verifier() -> String {
    random_alphanumeric(64)
}

/// S256 code challenge for a verifier: BASE64URL(SHA256(verifier)) without
/// padding (RFC 7636 §4.2).
pub fn code_challenge_s256(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(verifier.as_bytes());
    base64url_no_pad(&hash)
}

fn random_alphanumeric(len: usize) -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut out = String::with_capacity(len);
    // Rejection sampling (62*4 = 248) for uniform output.
    let mut buf = vec![0_u8; len * 2];
    if getrandom::fill(&mut buf).is_ok() {
        for byte in buf {
            if out.len() >= len {
                break;
            }
            if byte < 248 {
                out.push(ALPHABET[byte as usize % 62] as char);
            }
        }
    }
    // Practically unreachable tail: only when the OS RNG itself failed.
    let mut fallback = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    while out.len() < len {
        fallback = fallback
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(std::process::id() as u128);
        out.push(ALPHABET[(fallback % 62) as usize] as char);
    }
    out
}

fn base64url_no_pad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((bytes.len() * 4).div_ceil(3));
    for chunk in bytes.chunks(3) {
        let mut n = 0_u32;
        for (index, &byte) in chunk.iter().enumerate() {
            n |= (byte as u32) << (16 - 8 * index);
        }
        let chars = (chunk.len() * 8).div_ceil(6);
        for index in 0..chars {
            out.push(ALPHABET[((n >> (18 - 6 * index)) & 0x3F) as usize] as char);
        }
    }
    out
}

fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Runs the full interactive login with a caller-provided OAuth `state`, so the
/// UI can show the exact sign-in URL as a manual fallback while waiting.
/// Opens the browser, waits for the loopback callback, then exchanges the
/// code for tokens. Blocking; call on a worker thread, never on the egui UI
/// thread.
pub fn login_with_state_blocking(
    client: &reqwest::blocking::Client,
    backend_url: &str,
    state: &str,
    code_verifier: &str,
) -> Result<OauthSession> {
    let backend = validated_backend_base_url(backend_url)?;
    // Fail fast when the Worker itself reports bad/missing OAuth credentials
    // instead of sending the user to the browser first.
    check_backend_blocking(client, &backend)?;
    let url = authorize_url(&backend, state, &code_challenge_s256(code_verifier));
    open_browser(&url).with_context(|| format!("opening {url}"))?;
    let code =
        wait_for_callback(state).context("waiting for the osu! sign-in callback in the app")?;
    exchange_code_blocking(client, &backend, &code, code_verifier)
}

/// Asks the Worker whether its osu! OAuth credentials are present and accepted
/// by osu! before starting the interactive flow.
pub fn check_backend_blocking(client: &reqwest::blocking::Client, backend_url: &str) -> Result<()> {
    let backend = validated_backend_base_url(backend_url)?;
    let response = client
        .get(format!("{backend}/oauth/check"))
        .send()
        .map_err(|err| crate::updates::transport_error("reaching the sign-in backend", &err))?;
    let check: CheckResponse = response
        .json()
        .context("decoding the sign-in backend response")?;
    if check.ok {
        return Ok(());
    }
    anyhow::bail!(
        "{}",
        check
            .message
            .or(check.error)
            .unwrap_or_else(|| "the sign-in backend is not ready".to_owned())
    )
}

#[derive(Debug, Clone, Deserialize)]
struct CheckResponse {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// Rejects backend URLs that are not `http(s)` — launchers treat a schemeless
/// string as a file path, which opens the file manager instead of a browser.
pub fn validated_backend_base_url(backend_url: &str) -> Result<String> {
    let backend = backend_base_url(backend_url);
    if backend.is_empty() {
        anyhow::bail!("backend URL is required for osu! sign-in");
    }
    let lower = backend.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        anyhow::bail!("backend URL must start with http:// or https:// (got {backend:?})");
    }
    Ok(backend)
}

pub fn exchange_code_blocking(
    client: &reqwest::blocking::Client,
    backend_url: &str,
    code: &str,
    code_verifier: &str,
) -> Result<OauthSession> {
    let backend = backend_base_url(backend_url);
    let response = client
        .post(format!("{backend}/oauth/token"))
        .form(&[
            ("code", code),
            ("redirect_uri", OAUTH_REDIRECT_URI),
            ("code_verifier", code_verifier),
        ])
        .send()
        .map_err(|err| {
            crate::updates::transport_error("exchanging the osu! authorization code", &err)
        })?;
    token_session_from_response(response, "sign-in")
}

/// A failed refresh, classified so callers know whether the stored grant is
/// dead or the failure was transient.
#[derive(Debug)]
pub struct RefreshError {
    /// True only for `invalid_grant`: the refresh token itself was rejected
    /// and signing in again is the only fix. Network failures, worker
    /// misconfiguration, and osu! outages are transient — the stored session
    /// must be kept so the next attempt can succeed without re-login.
    pub permanent: bool,
    pub err: anyhow::Error,
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.err)
    }
}

impl std::error::Error for RefreshError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.err.as_ref())
    }
}

pub fn refresh_blocking(
    client: &reqwest::blocking::Client,
    backend_url: &str,
    refresh_token: &str,
) -> std::result::Result<OauthSession, RefreshError> {
    let backend = backend_base_url(backend_url);
    let response = client
        .post(format!("{backend}/oauth/refresh"))
        .form(&[("refresh_token", refresh_token)])
        .send()
        .map_err(|err| RefreshError {
            permanent: false,
            err: crate::updates::transport_error("refreshing the osu! access token", &err),
        })?;
    if response.status().is_success() {
        return response
            .json::<TokenResponse>()
            .context("decoding the osu! token response")
            .and_then(TokenResponse::into_session)
            .map_err(|err| RefreshError {
                permanent: false,
                err,
            });
    }
    let body = response.text().unwrap_or_default();
    Err(RefreshError {
        permanent: is_invalid_grant(&body),
        err: anyhow::anyhow!("{}", friendly_token_error(&body, "token refresh")),
    })
}

/// True when an osu! error body reports `invalid_grant` (revoked/expired
/// refresh token). Any other failure — including other OAuth error codes —
/// is treated as transient: only a dead grant discards the stored session.
fn is_invalid_grant(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|parsed| {
            parsed
                .get("error")?
                .as_str()
                .map(|error| error == "invalid_grant")
        })
        .unwrap_or(false)
}

/// Turns a proxied osu! token response into a session, translating osu!'s
/// terse error codes into actionable messages instead of raw JSON.
fn token_session_from_response(
    response: reqwest::blocking::Response,
    action: &str,
) -> Result<OauthSession> {
    if response.status().is_success() {
        return response
            .json::<TokenResponse>()
            .context("decoding the osu! token response")?
            .into_session();
    }
    let body = response.text().unwrap_or_default();
    anyhow::bail!("{}", friendly_token_error(&body, action))
}

fn friendly_token_error(body: &str, action: &str) -> String {
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let error = parsed
        .get("error")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let description = parsed
        .get("error_description")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    match error {
        "invalid_client" => format!(
            "osu! rejected the Worker's OAuth credentials (invalid_client{detail}). Fix: on the machine that deploys the Worker, run `npx wrangler secret put OSU_CLIENT_ID` and `npx wrangler secret put OSU_CLIENT_SECRET` with the values from https://osu.ppy.sh/home/account/edit#oauth, then `npx wrangler deploy`.",
            detail = if description.is_empty() {
                String::new()
            } else {
                format!(": {description}")
            },
        ),
        _ if !error.is_empty() => format!(
            "osu! {action} failed ({error}{detail})",
            detail = if description.is_empty() {
                String::new()
            } else {
                format!(": {description}")
            },
        ),
        _ => format!(
            "osu! {action} failed: {}",
            body.chars().take(300).collect::<String>()
        ),
    }
}

/// Refreshes the session in place when it is missing/expired/expiring soon.
/// Returns the usable access token, or `None` when there is no session.
pub fn ensure_access_token(
    client: &reqwest::blocking::Client,
    backend_url: &str,
    session: &mut Option<OauthSession>,
) -> Result<Option<String>> {
    let Some(current) = session.clone() else {
        return Ok(None);
    };
    if !current.needs_refresh() {
        return Ok(Some(current.access_token));
    }
    match refresh_blocking(client, backend_url, &current.refresh_token) {
        Ok(next) => {
            let token = next.access_token.clone();
            *session = Some(next);
            Ok(Some(token))
        }
        Err(failure) if failure.permanent => {
            *session = None;
            Err(failure.err)
        }
        Err(failure) => Err(failure.err),
    }
}

/// Waits for `GET /callback?code=...&state=...` on the loopback address and
/// returns the authorization code. Validates `state` to prevent CSRF.
pub fn wait_for_callback(expected_state: &str) -> Result<String> {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:3000").context("binding 127.0.0.1:3000")?;
    listener
        .set_nonblocking(true)
        .context("configuring the sign-in listener")?;
    let deadline = SystemTime::now() + OAUTH_CALLBACK_TIMEOUT;

    loop {
        if SystemTime::now() >= deadline {
            anyhow::bail!("timed out waiting for the osu! sign-in callback");
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if let Some(code) = handle_callback_connection(stream, expected_state)? {
                    return Ok(code);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(err).context("accepting the sign-in callback"),
        }
    }
}

fn handle_callback_connection(
    stream: std::net::TcpStream,
    expected_state: &str,
) -> Result<Option<String>> {
    let mut stream = stream;
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut reader = std::io::BufReader::new(stream.try_clone().context("reading callback")?);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .context("reading the sign-in callback")?;
    // Drain headers so the browser does not hang on keep-alive.
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok();
        if line.trim().is_empty() {
            break;
        }
    }

    let result = parse_callback_request(&request_line, expected_state);
    let (status, body) = match &result {
        Ok(_) => (
            "200 OK",
            "<h1>Signed in</h1><p>You can return to osu! Map Manager.</p>",
        ),
        Err(_) => (
            "400 Bad Request",
            "<h1>Sign-in failed</h1><p>Return to osu! Map Manager and try again.</p>",
        ),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).ok();
    stream.flush().ok();
    result.map(Some)
}

fn parse_callback_request(request_line: &str, expected_state: &str) -> Result<String> {
    let path = request_line
        .split_whitespace()
        .nth(1)
        .context("unexpected callback request")?;
    if !path.starts_with("/callback") {
        anyhow::bail!("unexpected callback path");
    }
    let query = path.split_once('?').map(|(_, query)| query).unwrap_or("");
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        match key {
            "code" => code = Some(url_decode(value)),
            "state" => state = Some(url_decode(value)),
            _ => {}
        }
    }
    let state = state.unwrap_or_default();
    if state != expected_state {
        anyhow::bail!("sign-in state mismatch; try again");
    }
    let code = code.unwrap_or_default();
    if code.trim().is_empty() {
        anyhow::bail!("osu! did not return an authorization code");
    }
    Ok(code)
}

fn url_decode(value: &str) -> String {
    let mut out = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                if let (Some(high), Some(low)) =
                    (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
                {
                    out.push(high * 16 + low);
                    index += 3;
                    continue;
                }
                out.push(b'%');
                index += 1;
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn open_browser(url: &str) -> Result<()> {
    // `webbrowser` uses the platform's URL handler (ShellExecute on Windows),
    // unlike launching explorer/open directly, which can misinterpret the
    // string as a file path and open the file manager instead.
    webbrowser::open(url).with_context(|| format!("opening {url}"))?;
    Ok(())
}

pub fn oauth_session_path(osu_root: &str) -> PathBuf {
    app_data_path(osu_root).join("oauth.json")
}

fn app_data_path(osu_root: &str) -> PathBuf {
    let root = if osu_root.trim().is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        expand_prefilled_path(osu_root)
    };
    root.join(".osu-map-manager")
}

fn expand_prefilled_path(path: &str) -> PathBuf {
    let path = path.trim();
    if let Some(user_profile) = std::env::var_os("USERPROFILE")
        && let Some(suffix) = path.strip_prefix("%USERPROFILE%")
    {
        let suffix = suffix.trim_start_matches(['\\', '/']);
        return PathBuf::from(user_profile).join(suffix);
    }
    if let Some(rest) = path.strip_prefix('~')
        && (rest.is_empty() || rest.starts_with(['/', '\\']))
        && let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
    {
        let suffix = rest.trim_start_matches(['\\', '/']);
        return PathBuf::from(home).join(suffix);
    }
    PathBuf::from(path)
}

pub fn load_oauth_session(osu_root: &str) -> Option<OauthSession> {
    let path = oauth_session_path(osu_root);
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save_oauth_session(osu_root: &str, session: &OauthSession) -> Result<()> {
    let path = oauth_session_path(osu_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(session)?;
    crate::collection::write_atomic(&path, text.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    restrict_token_permissions(&path)
}

/// Refresh/access tokens are bearer credentials: limit the file to the owner
/// where the platform allows it. (Windows has no Unix mode bits; the file
/// already lives in the user's own osu! directory.)
fn restrict_token_permissions(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)
            .with_context(|| format!("reading permissions of {}", path.display()))?
            .permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(path, permissions)
            .with_context(|| format!("restricting permissions of {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

pub fn clear_oauth_session(osu_root: &str) {
    let path = oauth_session_path(osu_root);
    let _ = fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_points_at_worker_with_redirect_state_and_pkce() {
        let url = authorize_url("https://example.workers.dev/", "abc 123", "challenge-~_ABC");
        assert_eq!(
            url,
            "https://example.workers.dev/oauth/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A3000%2Fcallback&state=abc%20123&code_challenge=challenge-~_ABC&code_challenge_method=S256"
        );
    }

    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        // Appendix B of RFC 7636.
        assert_eq!(
            code_challenge_s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn state_and_verifier_are_unpredictable_and_well_formed() {
        let is_alphanumeric = |value: &str| value.bytes().all(|byte| byte.is_ascii_alphanumeric());
        let state_a = generate_state();
        let state_b = generate_state();
        assert_eq!(state_a.len(), 24);
        assert!(is_alphanumeric(&state_a));
        assert_ne!(state_a, state_b);

        let verifier = generate_code_verifier();
        assert_eq!(verifier.len(), 64);
        assert!(is_alphanumeric(&verifier));
        assert_eq!(code_challenge_s256(&verifier).len(), 43);
    }

    #[test]
    fn callback_parses_code_and_validates_state() {
        let line = "GET /callback?code=the-code&state=secret HTTP/1.1\r\n";
        assert_eq!(parse_callback_request(line, "secret").unwrap(), "the-code");
        assert!(parse_callback_request(line, "other").is_err());
        let missing = "GET /callback?state=secret HTTP/1.1\r\n";
        assert!(parse_callback_request(missing, "secret").is_err());
    }

    #[test]
    fn backend_url_requires_http_scheme() {
        assert!(validated_backend_base_url("").is_err());
        assert!(validated_backend_base_url("osu-map-manager.workers.dev").is_err());
        assert!(validated_backend_base_url("ftp://example.com").is_err());
        assert_eq!(
            validated_backend_base_url("https://example.workers.dev/").unwrap(),
            "https://example.workers.dev"
        );
        assert_eq!(
            validated_backend_base_url("http://localhost:8787").unwrap(),
            "http://localhost:8787"
        );
    }

    #[test]
    fn token_errors_are_actionable() {
        let body =
            r#"{"error":"invalid_client","error_description":"Client authentication failed"}"#;
        let message = friendly_token_error(body, "sign-in");
        assert!(message.contains("invalid_client"));
        assert!(message.contains("wrangler secret put OSU_CLIENT_SECRET"));

        let other = friendly_token_error(
            r#"{"error":"invalid_grant","error_description":"nope"}"#,
            "sign-in",
        );
        assert!(other.contains("invalid_grant"));

        let html = friendly_token_error("<html>oops</html>", "sign-in");
        assert!(html.contains("oops"));
    }

    #[test]
    fn only_invalid_grant_is_a_permanent_refresh_failure() {
        assert!(is_invalid_grant(
            r#"{"error":"invalid_grant","error_description":"The refresh token is invalid."}"#
        ));
        // Other OAuth errors (e.g. broken Worker secrets) and non-JSON
        // bodies are transient: the stored session must survive them.
        assert!(!is_invalid_grant(
            r#"{"error":"invalid_client","error_description":"Client authentication failed"}"#
        ));
        assert!(!is_invalid_grant("<html>bad gateway</html>"));
        assert!(!is_invalid_grant(""));
    }

    #[test]
    fn expired_session_needs_refresh() {
        let session = OauthSession {
            access_token: "token".to_owned(),
            refresh_token: "refresh".to_owned(),
            expires_at_unix: 1,
        };
        assert!(session.needs_refresh());
        let fresh = OauthSession {
            access_token: "token".to_owned(),
            refresh_token: "refresh".to_owned(),
            expires_at_unix: u64::MAX,
        };
        assert!(!fresh.needs_refresh());
    }
}
