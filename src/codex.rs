use crate::auth::SavedAuth;
use crate::error::ProxyError;
use futures_util::{Stream, StreamExt};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::Serialize;
use serde_json::Value;
use std::pin::Pin;

const CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

#[derive(Clone)]
pub struct CodexClient {
    http_client: reqwest::Client,
    responses_url: String,
}

#[derive(Debug, Serialize)]
pub struct CodexRequest {
    pub model: String,
    pub instructions: String,
    pub input: Vec<Value>,
    pub reasoning: CodexReasoning,
    pub stream: bool,
    pub store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CodexReasoning {
    pub effort: String,
}

pub type CodexStream = Pin<Box<dyn Stream<Item = Result<String, ProxyError>> + Send>>;

impl CodexClient {
    pub fn new() -> Self {
        Self {
            http_client: reqwest::Client::new(),
            responses_url: CODEX_RESPONSES_URL.to_string(),
        }
    }

    pub async fn stream_response(
        &self,
        saved_auth: &SavedAuth,
        upstream_request: &CodexRequest,
        conversation_id: &str,
    ) -> Result<CodexStream, ProxyError> {
        let response = self
            .http_client
            .post(&self.responses_url)
            .headers(codex_backend_headers(saved_auth, conversation_id)?)
            .json(upstream_request)
            .send()
            .await
            .map_err(ProxyError::UpstreamRequest)?;

        if !response.status().is_success() {
            return Err(upstream_status_error(response).await);
        }

        tracing::info!(
            status = response.status().as_u16(),
            "upstream streaming response"
        );
        Ok(Box::pin(response.bytes_stream().map(|chunk_result| {
            chunk_result
                .map(|chunk| String::from_utf8_lossy(&chunk).to_string())
                .map_err(ProxyError::UpstreamRequest)
        })))
    }
}

fn codex_backend_headers(
    saved_auth: &SavedAuth,
    conversation_id: &str,
) -> Result<HeaderMap, ProxyError> {
    let mut headers = HeaderMap::new();
    let mut authorization_header =
        HeaderValue::from_str(&format!("Bearer {}", saved_auth.access_token)).map_err(|error| {
            ProxyError::InvalidRequest(format!("invalid access token header: {error}"))
        })?;
    authorization_header.set_sensitive(true);
    headers.insert(AUTHORIZATION, authorization_header);
    headers.insert(
        "x-client-request-id",
        HeaderValue::from_str(conversation_id).map_err(|error| {
            ProxyError::InvalidRequest(format!("invalid conversation id header: {error}"))
        })?,
    );
    headers.insert(
        "session_id",
        HeaderValue::from_str(conversation_id).map_err(|error| {
            ProxyError::InvalidRequest(format!("invalid session id header: {error}"))
        })?,
    );

    if let Some(account_id) = &saved_auth.account_id {
        headers.insert(
            "ChatGPT-Account-Id",
            HeaderValue::from_str(account_id).map_err(|error| {
                ProxyError::InvalidRequest(format!("invalid account id header: {error}"))
            })?,
        );
    }

    Ok(headers)
}

async fn upstream_status_error(response: reqwest::Response) -> ProxyError {
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    tracing::warn!(status, body, "upstream request failed");
    ProxyError::UpstreamStatus { status, body }
}
