use crate::auth::AuthManager;
use crate::codex::{CodexClient, CodexStream};
use crate::config::ProxySettings;
use crate::error::ProxyError;
use crate::openai::{
    ChatCompletionRequest, content_stream_chunk, final_stream_chunk, first_stream_chunk,
    models_response,
};
use axum::body::{Body, Bytes};
use axum::extract::DefaultBodyLimit;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::header::{AUTHORIZATION, CONTENT_LENGTH, USER_AGENT};
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
use axum::middleware::{Next, from_fn};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use futures_util::{Stream, StreamExt};
use rand::RngCore;
use serde_json::Value;
use std::convert::Infallible;
use std::env;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::time::Instant;

pub struct ServerConfig {
    pub host: IpAddr,
    pub port: u16,
    pub auth_manager: AuthManager,
    pub settings: ProxySettings,
}

#[derive(Clone)]
struct AppState {
    auth_manager: AuthManager,
    codex_client: CodexClient,
    conversation_id: String,
    settings: ProxySettings,
}

pub async fn run_server(server_config: ServerConfig) -> anyhow::Result<()> {
    let server_address = SocketAddr::from((server_config.host, server_config.port));
    let tcp_listener = tokio::net::TcpListener::bind(server_address).await?;
    let bound_address = tcp_listener.local_addr()?;

    tracing::info!("codex-proxy listening on http://{bound_address}/v1");
    if let Some(system_prompt) = server_config.settings.injected_system_prompt()? {
        tracing::info!(
            system_prompt_file = %server_config.settings.system_prompt_file.display(),
            system_prompt_chars = system_prompt.chars().count(),
            system_prompt = %system_prompt,
            "using injected system prompt"
        );
    }
    let conversation_id = new_conversation_id();
    tracing::info!(conversation_id, "using codex prompt cache key");

    axum::serve(
        tcp_listener,
        router(AppState {
            auth_manager: server_config.auth_manager,
            codex_client: CodexClient::new(),
            conversation_id,
            settings: server_config.settings,
        }),
    )
    .await?;

    Ok(())
}

fn router(app_state: AppState) -> Router {
    Router::new()
        .route("/v1/models", get(handle_models))
        .route("/v1/chat/completions", post(handle_chat_completions))
        .layer(DefaultBodyLimit::max(25 * 1024 * 1024))
        .layer(from_fn(log_request))
        .with_state(app_state)
}

async fn handle_models() -> Json<crate::openai::ModelsResponse> {
    tracing::info!("models request");
    Json(models_response())
}

async fn handle_chat_completions(
    State(app_state): State<AppState>,
    request_headers: HeaderMap,
    request_body: Bytes,
) -> Result<Response, ProxyError> {
    require_proxy_api_key(&request_headers, &app_state.settings)?;

    log_json_body(
        "client chat completion body",
        &request_body,
        app_state.settings.detailed_logs,
    );

    let chat_completion_request: ChatCompletionRequest = serde_json::from_slice(&request_body)
        .map_err(|error| {
            tracing::warn!(error = %error, "failed to parse chat completion request");
            ProxyError::InvalidRequest(format!("invalid JSON chat completion request: {error}"))
        })?;
    let request_stats = chat_request_stats(&chat_completion_request);

    tracing::info!(
        model = %chat_completion_request.model,
        stream = chat_completion_request.stream,
        messages = chat_completion_request.messages.len(),
        roles = request_stats.roles,
        text_chars = request_stats.text_chars,
        image_parts = request_stats.image_parts,
        "chat completion request"
    );

    let mut upstream_request = chat_completion_request.to_codex_request(&app_state.settings)?;
    upstream_request.prompt_cache_key = Some(app_state.conversation_id.clone());
    let upstream_request_json = serde_json::to_value(&upstream_request)
        .expect("upstream request should serialize for diagnostics");
    tracing::info!(
        upstream_model = %upstream_request.model,
        upstream_stream = upstream_request.stream,
        upstream_service_tier = upstream_request.service_tier.as_deref().unwrap_or("none"),
        upstream_reasoning_effort = %upstream_request.reasoning.effort,
        upstream_input_items = upstream_request.input.len(),
        upstream_images = count_input_images(&upstream_request_json),
        "forwarding codex request"
    );
    log_json_value(
        "upstream codex request body",
        &upstream_request_json,
        app_state.settings.detailed_logs,
    );

    let saved_auth = app_state.auth_manager.access_token().await?;

    if chat_completion_request.stream {
        let upstream_stream = app_state
            .codex_client
            .stream_response(&saved_auth, &upstream_request, &app_state.conversation_id)
            .await?;
        return Ok(openai_stream_response(
            &chat_completion_request.model,
            upstream_stream,
        ));
    }

    let upstream_stream = app_state
        .codex_client
        .stream_response(&saved_auth, &upstream_request, &app_state.conversation_id)
        .await?;
    let response_text = collect_openai_response_text(upstream_stream).await?;
    tracing::info!(
        response_text_chars = response_text.chars().count(),
        "collected non-streaming response"
    );
    Ok((
        StatusCode::OK,
        Json(crate::openai::chat_completion_response(
            &chat_completion_request.model,
            response_text,
        )),
    )
        .into_response())
}

fn require_proxy_api_key(
    request_headers: &HeaderMap,
    settings: &ProxySettings,
) -> Result<(), ProxyError> {
    let Some(api_key) = bearer_token(request_headers) else {
        tracing::warn!("chat completion request missing proxy API key");
        return Err(ProxyError::InvalidProxyApiKey);
    };

    if settings
        .api_keys
        .iter()
        .any(|allowed_key| allowed_key == api_key)
    {
        return Ok(());
    }

    tracing::warn!("chat completion request used invalid proxy API key");
    Err(ProxyError::InvalidProxyApiKey)
}

fn bearer_token(request_headers: &HeaderMap) -> Option<&str> {
    request_headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .filter(|api_key| !api_key.is_empty())
}

fn new_conversation_id() -> String {
    let mut random_bytes = [0_u8; 18];
    rand::thread_rng().fill_bytes(&mut random_bytes);
    format!(
        "codex-proxy-{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes)
    )
}

async fn log_request(request: Request<Body>, next: Next) -> Response {
    let started_at = Instant::now();
    let method = request.method().clone();
    let uri = request.uri().clone();
    let content_length = request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    let user_agent = request
        .headers()
        .get(USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("unknown")
        .to_string();

    tracing::info!(
        %method,
        %uri,
        content_length,
        user_agent,
        "incoming request"
    );

    let response = next.run(request).await;
    tracing::info!(
        %method,
        %uri,
        status = response.status().as_u16(),
        latency_ms = started_at.elapsed().as_millis(),
        "request completed"
    );
    response
}

fn openai_stream_response(client_model: &str, upstream_stream: CodexStream) -> Response {
    let client_model = client_model.to_string();
    let first_chunk = serde_json::to_string(&first_stream_chunk(&client_model))
        .expect("first stream chunk should serialize");
    let final_chunk = serde_json::to_string(&final_stream_chunk(&client_model))
        .expect("final stream chunk should serialize");
    let stream = futures_util::stream::once(async move {
        Ok::<_, Infallible>(Bytes::from(format!("data: {first_chunk}\n\n")))
    })
    .chain(openai_content_stream(client_model, upstream_stream))
    .chain(futures_util::stream::once(async move {
        Ok::<_, Infallible>(Bytes::from(format!("data: {final_chunk}\n\n")))
    }))
    .chain(futures_util::stream::once(async {
        Ok::<_, Infallible>(Bytes::from("data: [DONE]\n\n"))
    }));

    let mut response = Body::from_stream(stream).into_response();
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    response
}

async fn collect_openai_response_text(
    mut upstream_stream: CodexStream,
) -> Result<String, ProxyError> {
    let mut upstream_buffer = String::new();
    let mut response_text = String::new();

    while let Some(upstream_chunk_result) = upstream_stream.next().await {
        response_text.push_str(
            &extract_text_deltas_from_buffer(&mut upstream_buffer, &upstream_chunk_result?)
                .join(""),
        );
    }

    if !upstream_buffer.is_empty() {
        response_text.push_str(&extract_text_deltas(&upstream_buffer).join(""));
    }

    Ok(response_text)
}

fn openai_content_stream(
    client_model: String,
    upstream_stream: CodexStream,
) -> Pin<Box<dyn Stream<Item = Result<Bytes, Infallible>> + Send>> {
    Box::pin(
        upstream_stream
            .scan(String::new(), move |upstream_buffer, chunk_result| {
                let client_model = client_model.clone();
                futures_util::future::ready(Some(match chunk_result {
                    Ok(upstream_chunk) => {
                        let openai_chunks =
                            extract_text_deltas_from_buffer(upstream_buffer, &upstream_chunk)
                                .into_iter()
                                .map(|text_delta| {
                                    let openai_chunk = serde_json::to_string(
                                        &content_stream_chunk(&client_model, text_delta),
                                    )
                                    .expect("content stream chunk should serialize");
                                    format!("data: {openai_chunk}\n\n")
                                })
                                .collect::<String>();
                        Ok(Bytes::from(openai_chunks))
                    }
                    Err(error) => {
                        let openai_chunk = serde_json::json!({
                            "error": {
                                "message": error.to_string(),
                                "type": "proxy_stream_error",
                                "code": null,
                            }
                        });
                        Ok(Bytes::from(format!("data: {openai_chunk}\n\n")))
                    }
                }))
            })
            .filter(|chunk_result| {
                futures_util::future::ready(match chunk_result {
                    Ok(bytes) => !bytes.is_empty(),
                    Err(error) => match *error {},
                })
            }),
    )
}

fn extract_text_deltas_from_buffer(
    upstream_buffer: &mut String,
    upstream_chunk: &str,
) -> Vec<String> {
    upstream_buffer.push_str(upstream_chunk);
    let Some(last_newline_index) = upstream_buffer.rfind('\n') else {
        return Vec::new();
    };

    let complete_events = upstream_buffer[..=last_newline_index].to_string();
    let remaining_events = upstream_buffer[last_newline_index + 1..].to_string();
    *upstream_buffer = remaining_events;
    extract_text_deltas(&complete_events)
}

fn extract_text_deltas(upstream_chunk: &str) -> Vec<String> {
    upstream_chunk
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|line| *line != "[DONE]")
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(text_delta_from_event)
        .collect()
}

fn text_delta_from_event(event_value: Value) -> Option<String> {
    let event_type = event_value.get("type").and_then(Value::as_str);
    if event_type.is_some_and(|event_type| !event_type.ends_with(".delta")) {
        return None;
    }

    event_value
        .get("delta")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            event_value
                .get("text")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .or_else(|| {
            event_value
                .get("output_text")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
}

struct ChatRequestStats {
    roles: String,
    text_chars: usize,
    image_parts: usize,
}

fn chat_request_stats(chat_completion_request: &ChatCompletionRequest) -> ChatRequestStats {
    let mut text_chars = 0;
    let mut image_parts = 0;
    let roles = chat_completion_request
        .messages
        .iter()
        .map(|message| {
            match &message.content {
                crate::openai::ChatMessageContent::Text(text) => {
                    text_chars += text.chars().count();
                }
                crate::openai::ChatMessageContent::Parts(parts) => {
                    for part in parts {
                        match part {
                            crate::openai::ChatMessageContentPart::Text { text } => {
                                text_chars += text.chars().count();
                            }
                            crate::openai::ChatMessageContentPart::ImageUrl { .. } => {
                                image_parts += 1;
                            }
                        }
                    }
                }
            }
            message.role.as_str()
        })
        .collect::<Vec<_>>()
        .join(",");

    ChatRequestStats {
        roles,
        text_chars,
        image_parts,
    }
}

fn log_json_body(message: &str, body: &Bytes, detailed_logs: bool) {
    if !log_bodies_enabled(detailed_logs) {
        return;
    }

    match serde_json::from_slice::<Value>(body) {
        Ok(value) => log_json_value(message, &value, detailed_logs),
        Err(error) => tracing::info!(
            error = %error,
            body_bytes = body.len(),
            diagnostic = message,
            "failed to log JSON body"
        ),
    }
}

fn log_json_value(message: &str, value: &Value, detailed_logs: bool) {
    if !log_bodies_enabled(detailed_logs) {
        return;
    }

    tracing::info!(
        body = %sanitize_json_value(value),
        diagnostic = message,
        "JSON body"
    );
}

fn log_bodies_enabled(detailed_logs: bool) -> bool {
    if detailed_logs {
        return true;
    }

    matches!(
        env::var("CODEX_PROXY_LOG_BODIES").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
    )
}

fn sanitize_json_value(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(sanitize_json_value).collect()),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    if key.contains("token")
                        || key.contains("authorization")
                        || key.contains("auth")
                    {
                        return (key.clone(), Value::String("<redacted>".to_string()));
                    }

                    (key.clone(), sanitize_json_field(key, value))
                })
                .collect(),
        ),
        Value::String(text) => Value::String(sanitize_string(text)),
        _ => value.clone(),
    }
}

fn sanitize_json_field(key: &str, value: &Value) -> Value {
    match value {
        Value::String(text) if key == "url" || key == "image_url" => {
            Value::String(sanitize_string(text))
        }
        _ => sanitize_json_value(value),
    }
}

fn sanitize_string(text: &str) -> String {
    let Some((mime_type, base64_data)) = text.split_once(";base64,") else {
        return text.to_string();
    };

    if !mime_type.starts_with("data:") {
        return text.to_string();
    }

    format!("{mime_type};base64,<redacted {} chars>", base64_data.len())
}

fn count_input_images(value: &Value) -> usize {
    match value {
        Value::Array(items) => items.iter().map(count_input_images).sum(),
        Value::Object(object) => {
            usize::from(
                object
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|item_type| item_type == "input_image"),
            ) + object.values().map(count_input_images).sum::<usize>()
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthManager;

    #[test]
    fn default_bind_host_is_public_in_cli_contract() {
        let default_host: IpAddr = "0.0.0.0".parse().expect("public bind should parse");
        assert_eq!(default_host, IpAddr::from([0, 0, 0, 0]));
    }

    #[test]
    fn conversation_id_is_codex_proxy_cache_key() {
        let conversation_id = new_conversation_id();

        assert!(conversation_id.starts_with("codex-proxy-"));
        assert!(conversation_id.len() > "codex-proxy-".len());
    }

    #[test]
    fn stream_delta_extractor_reads_common_event_shapes() {
        let upstream_chunk = [
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}",
            "data: {\"text\":\" world\"}",
            "data: [DONE]",
        ]
        .join("\n");

        assert_eq!(
            extract_text_deltas(&upstream_chunk),
            vec!["hello".to_string(), " world".to_string()]
        );
    }

    #[test]
    fn stream_delta_extractor_ignores_completed_full_content() {
        let upstream_chunk = [
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"cure-all\"}",
            "data: {\"type\":\"response.completed\",\"content\":[{\"text\":\"cure-all\"}]}",
            "data: [DONE]",
        ]
        .join("\n");

        assert_eq!(
            extract_text_deltas(&upstream_chunk),
            vec!["cure-all".to_string()]
        );
    }

    #[test]
    fn stream_delta_extractor_keeps_split_event_buffered() {
        let mut upstream_buffer = String::new();

        assert!(
            extract_text_deltas_from_buffer(&mut upstream_buffer, "data: {\"delta\":\"hel")
                .is_empty()
        );
        assert_eq!(
            extract_text_deltas_from_buffer(&mut upstream_buffer, "lo\"}\n"),
            vec!["hello".to_string()]
        );
    }

    #[test]
    fn chat_requests_require_allowed_proxy_api_key() {
        let mut headers = HeaderMap::new();
        let mut settings = ProxySettings::default();
        settings.api_keys = vec!["cp_allowed".to_string()];

        assert!(matches!(
            require_proxy_api_key(&headers, &settings),
            Err(ProxyError::InvalidProxyApiKey)
        ));

        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer cp_allowed"));

        assert!(require_proxy_api_key(&headers, &settings).is_ok());
    }

    #[tokio::test]
    async fn models_route_does_not_require_proxy_api_key() {
        let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test server should bind");
        let server_address = tcp_listener
            .local_addr()
            .expect("test server address should be available");
        let app = router(AppState {
            auth_manager: AuthManager::new(std::path::PathBuf::from("/tmp/missing-auth.json")),
            codex_client: CodexClient::new(),
            conversation_id: "codex-proxy-test".to_string(),
            settings: ProxySettings::default(),
        });
        tokio::spawn(async move {
            axum::serve(tcp_listener, app)
                .await
                .expect("test server should run");
        });

        let response = reqwest::get(format!("http://{server_address}/v1/models"))
            .await
            .expect("models request should succeed");

        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            !response
                .text()
                .await
                .expect("models response body should read")
                .contains("api_key")
        );
    }
}
