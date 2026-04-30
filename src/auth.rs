use crate::error::ProxyError;
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

const CHATGPT_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CHATGPT_OAUTH_ISSUER: &str = "https://auth.openai.com";
const BROWSER_LOGIN_PORT: u16 = 1455;
const DEVICE_LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const TOKEN_REFRESH_MARGIN: u64 = 30_000;

#[derive(Clone)]
pub struct AuthManager {
    auth_file: PathBuf,
    http_client: reqwest::Client,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SavedAuth {
    pub refresh_token: String,
    pub access_token: String,
    pub expires_at_millis: u64,
    pub account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    refresh_token: Option<String>,
    access_token: String,
    id_token: Option<String>,
    expires_in: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_auth_id: String,
    user_code: String,
    interval: String,
}

#[derive(Debug, Deserialize)]
struct DeviceTokenPollResponse {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Debug, Deserialize)]
struct TokenClaims {
    chatgpt_account_id: Option<String>,
    organizations: Option<Vec<TokenOrganization>>,
    #[serde(rename = "https://api.openai.com/auth")]
    openai_auth: Option<OpenAiAuthClaims>,
}

#[derive(Debug, Deserialize)]
struct TokenOrganization {
    id: String,
}

#[derive(Debug, Deserialize)]
struct OpenAiAuthClaims {
    chatgpt_account_id: Option<String>,
}

struct PkceCodes {
    code_verifier: String,
    code_challenge: String,
}

impl AuthManager {
    pub fn new(auth_file: PathBuf) -> Self {
        Self {
            auth_file,
            http_client: reqwest::Client::new(),
        }
    }

    pub fn default_auth_file() -> PathBuf {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".codex-proxy")
            .join("auth.json")
    }

    pub fn auth_file(&self) -> &PathBuf {
        &self.auth_file
    }

    pub fn load_saved_auth(&self) -> Result<SavedAuth, ProxyError> {
        let auth_json = std::fs::read_to_string(&self.auth_file).map_err(|read_error| {
            if read_error.kind() == std::io::ErrorKind::NotFound {
                ProxyError::MissingAuth
            } else {
                ProxyError::ReadAuth(read_error)
            }
        })?;
        serde_json::from_str(&auth_json).map_err(ProxyError::ParseAuth)
    }

    pub fn save_auth(&self, saved_auth: &SavedAuth) -> Result<(), ProxyError> {
        if let Some(auth_dir) = self.auth_file.parent() {
            std::fs::create_dir_all(auth_dir).map_err(ProxyError::WriteAuth)?;
        }

        let mut open_options = OpenOptions::new();
        open_options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open_options.mode(0o600);
        }

        let auth_json = serde_json::to_string_pretty(saved_auth).map_err(ProxyError::ParseToken)?;
        let mut auth_file = open_options
            .open(&self.auth_file)
            .map_err(ProxyError::WriteAuth)?;
        auth_file
            .write_all(auth_json.as_bytes())
            .map_err(ProxyError::WriteAuth)?;
        auth_file.flush().map_err(ProxyError::WriteAuth)
    }

    pub fn delete_auth(&self) -> Result<bool, ProxyError> {
        match std::fs::remove_file(&self.auth_file) {
            Ok(()) => Ok(true),
            Err(remove_error) if remove_error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(remove_error) => Err(ProxyError::WriteAuth(remove_error)),
        }
    }

    pub async fn access_token(&self) -> Result<SavedAuth, ProxyError> {
        let saved_auth = self.load_saved_auth()?;
        if saved_auth.expires_at_millis > now_millis() + TOKEN_REFRESH_MARGIN {
            return Ok(saved_auth);
        }

        tracing::info!("refreshing codex access token");
        let token_response = self.refresh_access_token(&saved_auth.refresh_token).await?;
        let refreshed_auth = self.saved_auth_from_tokens(token_response, saved_auth.account_id)?;
        self.save_auth(&refreshed_auth)?;
        tracing::info!("codex access token refreshed");
        Ok(refreshed_auth)
    }

    pub async fn login_with_browser(&self) -> Result<SavedAuth, ProxyError> {
        let pkce_codes = generate_pkce_codes();
        let state = generate_random_url_safe_string(32);
        let tcp_listener =
            TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], BROWSER_LOGIN_PORT)))
                .await
                .map_err(|error| {
                    ProxyError::Login(format!("failed to bind browser callback server: {error}"))
                })?;
        let redirect_uri = format!("http://localhost:{BROWSER_LOGIN_PORT}/auth/callback");
        let authorize_url = build_authorize_url(&redirect_uri, &pkce_codes, &state);

        println!("Open this URL to sign in:\n{authorize_url}\n");
        open_browser(&authorize_url);

        let authorization_code = wait_for_browser_code(tcp_listener, &state).await?;
        let token_response = self
            .exchange_authorization_code(
                &authorization_code,
                &redirect_uri,
                &pkce_codes.code_verifier,
            )
            .await?;
        let saved_auth = self.saved_auth_from_tokens(token_response, None)?;
        self.save_auth(&saved_auth)?;
        tracing::info!("browser login completed");
        Ok(saved_auth)
    }

    pub async fn login_with_device_code(&self) -> Result<SavedAuth, ProxyError> {
        let device_code_response = self.request_device_code().await?;
        println!(
            "Open {}/codex/device and enter code: {}",
            CHATGPT_OAUTH_ISSUER, device_code_response.user_code
        );

        let started_at = std::time::Instant::now();
        let polling_interval = Duration::from_secs(
            device_code_response
                .interval
                .parse::<u64>()
                .unwrap_or(5)
                .max(1)
                + 3,
        );

        while started_at.elapsed() < DEVICE_LOGIN_TIMEOUT {
            match self.poll_device_code(&device_code_response).await? {
                Some(device_token_poll_response) => {
                    let token_response = self
                        .exchange_authorization_code(
                            &device_token_poll_response.authorization_code,
                            &format!("{CHATGPT_OAUTH_ISSUER}/deviceauth/callback"),
                            &device_token_poll_response.code_verifier,
                        )
                        .await?;
                    let saved_auth = self.saved_auth_from_tokens(token_response, None)?;
                    self.save_auth(&saved_auth)?;
                    tracing::info!("device login completed");
                    return Ok(saved_auth);
                }
                None => tokio::time::sleep(polling_interval).await,
            }
        }

        Err(ProxyError::Login(
            "device login timed out after 15 minutes".to_string(),
        ))
    }

    async fn request_device_code(&self) -> Result<DeviceCodeResponse, ProxyError> {
        let response = self
            .http_client
            .post(format!(
                "{CHATGPT_OAUTH_ISSUER}/api/accounts/deviceauth/usercode"
            ))
            .json(&serde_json::json!({ "client_id": CHATGPT_OAUTH_CLIENT_ID }))
            .send()
            .await
            .map_err(ProxyError::OAuthRequest)?;

        if !response.status().is_success() {
            return Err(oauth_status_error(response).await);
        }

        response.json().await.map_err(ProxyError::OAuthRequest)
    }

    async fn poll_device_code(
        &self,
        device_code_response: &DeviceCodeResponse,
    ) -> Result<Option<DeviceTokenPollResponse>, ProxyError> {
        let response = self
            .http_client
            .post(format!(
                "{CHATGPT_OAUTH_ISSUER}/api/accounts/deviceauth/token"
            ))
            .json(&serde_json::json!({
                "device_auth_id": device_code_response.device_auth_id,
                "user_code": device_code_response.user_code,
            }))
            .send()
            .await
            .map_err(ProxyError::OAuthRequest)?;

        if response.status().as_u16() == 403 || response.status().as_u16() == 404 {
            return Ok(None);
        }

        if !response.status().is_success() {
            return Err(oauth_status_error(response).await);
        }

        response
            .json()
            .await
            .map(Some)
            .map_err(ProxyError::OAuthRequest)
    }

    async fn exchange_authorization_code(
        &self,
        authorization_code: &str,
        redirect_uri: &str,
        code_verifier: &str,
    ) -> Result<TokenResponse, ProxyError> {
        let token_request_body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", authorization_code)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("client_id", CHATGPT_OAUTH_CLIENT_ID)
            .append_pair("code_verifier", code_verifier)
            .finish();
        let response = self
            .http_client
            .post(format!("{CHATGPT_OAUTH_ISSUER}/oauth/token"))
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(token_request_body)
            .send()
            .await
            .map_err(ProxyError::OAuthRequest)?;

        if !response.status().is_success() {
            return Err(oauth_status_error(response).await);
        }

        response.json().await.map_err(ProxyError::OAuthRequest)
    }

    async fn refresh_access_token(&self, refresh_token: &str) -> Result<TokenResponse, ProxyError> {
        let token_refresh_body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "refresh_token")
            .append_pair("refresh_token", refresh_token)
            .append_pair("client_id", CHATGPT_OAUTH_CLIENT_ID)
            .finish();
        let response = self
            .http_client
            .post(format!("{CHATGPT_OAUTH_ISSUER}/oauth/token"))
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(token_refresh_body)
            .send()
            .await
            .map_err(ProxyError::OAuthRequest)?;

        if !response.status().is_success() {
            return Err(oauth_status_error(response).await);
        }

        response.json().await.map_err(ProxyError::OAuthRequest)
    }

    fn saved_auth_from_tokens(
        &self,
        token_response: TokenResponse,
        existing_account_id: Option<String>,
    ) -> Result<SavedAuth, ProxyError> {
        let refresh_token = token_response
            .refresh_token
            .clone()
            .or_else(|| {
                self.load_saved_auth()
                    .ok()
                    .map(|saved_auth| saved_auth.refresh_token)
            })
            .ok_or(ProxyError::MissingRefreshToken)?;
        let account_id = extract_account_id(&token_response).or(existing_account_id);

        Ok(SavedAuth {
            refresh_token,
            access_token: token_response.access_token,
            expires_at_millis: now_millis() + token_response.expires_in.unwrap_or(3600) * 1000,
            account_id,
        })
    }
}

async fn oauth_status_error(response: reqwest::Response) -> ProxyError {
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    ProxyError::OAuthStatus { status, body }
}

async fn wait_for_browser_code(
    tcp_listener: TcpListener,
    expected_state: &str,
) -> Result<String, ProxyError> {
    let (mut tcp_stream, _) = tcp_listener.accept().await.map_err(|error| {
        ProxyError::Login(format!("failed to accept browser callback: {error}"))
    })?;
    let mut request_buffer = vec![0; 8192];
    let bytes_read = tcp_stream
        .read(&mut request_buffer)
        .await
        .map_err(|error| ProxyError::Login(format!("failed to read browser callback: {error}")))?;
    let http_request = String::from_utf8_lossy(&request_buffer[..bytes_read]);
    let request_path = http_request
        .lines()
        .next()
        .and_then(|request_line| request_line.split_whitespace().nth(1))
        .ok_or_else(|| {
            ProxyError::Login("browser callback was not a valid HTTP request".to_string())
        })?;
    let callback_url = Url::parse(&format!("http://localhost{request_path}"))
        .map_err(|error| ProxyError::Login(format!("browser callback URL was invalid: {error}")))?;
    let query_pairs = callback_url
        .query_pairs()
        .into_owned()
        .collect::<HashMap<String, String>>();

    let response_html = "<!doctype html><title>Codex Proxy Login</title><h1>Login complete</h1><p>You can close this window.</p>";
    let http_response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        response_html.len(),
        response_html
    );
    tcp_stream
        .write_all(http_response.as_bytes())
        .await
        .map_err(|error| {
            ProxyError::Login(format!(
                "failed to write browser callback response: {error}"
            ))
        })?;

    if query_pairs.get("state").map(String::as_str) != Some(expected_state) {
        return Err(ProxyError::Login(
            "browser callback state did not match".to_string(),
        ));
    }

    query_pairs
        .get("code")
        .cloned()
        .ok_or_else(|| ProxyError::Login("browser callback did not include a code".to_string()))
}

fn build_authorize_url(redirect_uri: &str, pkce_codes: &PkceCodes, state: &str) -> String {
    let mut authorize_url = Url::parse(&format!("{CHATGPT_OAUTH_ISSUER}/oauth/authorize"))
        .expect("static oauth authorize URL should parse");
    authorize_url
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CHATGPT_OAUTH_CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", "openid profile email offline_access")
        .append_pair("code_challenge", &pkce_codes.code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", state)
        .append_pair("originator", "opencode");
    authorize_url.to_string()
}

fn generate_pkce_codes() -> PkceCodes {
    let code_verifier = generate_random_url_safe_string(32);
    let code_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(code_verifier.as_bytes()));
    PkceCodes {
        code_verifier,
        code_challenge,
    }
}

fn generate_random_url_safe_string(byte_count: usize) -> String {
    let mut random_bytes = vec![0; byte_count];
    rand::thread_rng().fill_bytes(&mut random_bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes)
}

fn extract_account_id(token_response: &TokenResponse) -> Option<String> {
    token_response
        .id_token
        .as_deref()
        .and_then(parse_jwt_claims)
        .and_then(account_id_from_claims)
        .or_else(|| parse_jwt_claims(&token_response.access_token).and_then(account_id_from_claims))
}

fn parse_jwt_claims(token: &str) -> Option<TokenClaims> {
    let token_payload = token.split('.').nth(1)?;
    let token_payload_json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token_payload)
        .ok()?;
    serde_json::from_slice(&token_payload_json).ok()
}

fn account_id_from_claims(token_claims: TokenClaims) -> Option<String> {
    token_claims
        .chatgpt_account_id
        .or_else(|| token_claims.openai_auth?.chatgpt_account_id)
        .or_else(|| {
            token_claims
                .organizations?
                .into_iter()
                .next()
                .map(|org| org.id)
        })
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn open_browser(authorize_url: &str) {
    #[cfg(target_os = "macos")]
    let open_result = std::process::Command::new("open")
        .arg(authorize_url)
        .spawn();
    #[cfg(target_os = "linux")]
    let open_result = std::process::Command::new("xdg-open")
        .arg(authorize_url)
        .spawn();
    #[cfg(target_os = "windows")]
    let open_result = std::process::Command::new("cmd")
        .args(["/C", "start", authorize_url])
        .spawn();

    if let Err(open_error) = open_result {
        tracing::warn!("could not open browser automatically: {open_error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_id_uses_direct_claim_first() {
        let account_id = account_id_from_claims(TokenClaims {
            chatgpt_account_id: Some("account-direct".to_string()),
            openai_auth: Some(OpenAiAuthClaims {
                chatgpt_account_id: Some("account-nested".to_string()),
            }),
            organizations: Some(vec![TokenOrganization {
                id: "org-first".to_string(),
            }]),
        });

        assert_eq!(account_id.as_deref(), Some("account-direct"));
    }

    #[test]
    fn saved_auth_defaults_expiration() {
        let auth_manager = AuthManager::new(PathBuf::from("/tmp/auth.json"));
        let saved_auth = auth_manager
            .saved_auth_from_tokens(
                TokenResponse {
                    refresh_token: Some("refresh".to_string()),
                    access_token: "access".to_string(),
                    id_token: None,
                    expires_in: None,
                },
                None,
            )
            .expect("token response should create saved auth");

        assert_eq!(saved_auth.refresh_token, "refresh");
        assert_eq!(saved_auth.access_token, "access");
        assert!(saved_auth.expires_at_millis > now_millis());
    }
}
