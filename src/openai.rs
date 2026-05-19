use crate::codex::{CodexReasoning, CodexRequest};
use crate::config::{ProxySettings, ReasoningEffort, Speed};
use crate::error::ProxyError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub stream: bool,
    pub service_tier: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Debug, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: ChatMessageContent,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ChatMessageContent {
    Text(String),
    Parts(Vec<ChatMessageContentPart>),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum ChatMessageContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrlPart },
}

#[derive(Debug, Deserialize)]
pub struct ImageUrlPart {
    pub url: String,
}

#[derive(Debug, Serialize)]
pub struct ModelsResponse {
    pub object: &'static str,
    pub data: Vec<ModelResponse>,
}

#[derive(Debug, Serialize)]
pub struct ModelResponse {
    pub id: &'static str,
    pub object: &'static str,
    pub created: u64,
    pub owned_by: &'static str,
}

#[derive(Debug, Serialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChatCompletionChoice>,
}

#[derive(Debug, Serialize)]
pub struct ChatCompletionChoice {
    pub index: u32,
    pub message: ChatCompletionMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ChatCompletionMessage {
    pub role: &'static str,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChatCompletionChunkChoice>,
}

#[derive(Debug, Serialize)]
pub struct ChatCompletionChunkChoice {
    pub index: u32,
    pub delta: ChatCompletionDelta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ChatCompletionDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

pub fn models_response() -> ModelsResponse {
    ModelsResponse {
        object: "list",
        data: vec![model_response("gpt-5.5"), model_response("gpt-5.5-fast")],
    }
}

pub fn chat_completion_response(client_model: &str, content: String) -> ChatCompletionResponse {
    ChatCompletionResponse {
        id: format!("chatcmpl-{}", now_seconds()),
        object: "chat.completion",
        created: now_seconds(),
        model: client_model.to_string(),
        choices: vec![ChatCompletionChoice {
            index: 0,
            message: ChatCompletionMessage {
                role: "assistant",
                content,
            },
            finish_reason: Some("stop".to_string()),
        }],
    }
}

pub fn first_stream_chunk(client_model: &str) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: format!("chatcmpl-{}", now_seconds()),
        object: "chat.completion.chunk",
        created: now_seconds(),
        model: client_model.to_string(),
        choices: vec![ChatCompletionChunkChoice {
            index: 0,
            delta: ChatCompletionDelta {
                role: Some("assistant"),
                content: None,
            },
            finish_reason: None,
        }],
    }
}

pub fn content_stream_chunk(client_model: &str, content: String) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: format!("chatcmpl-{}", now_seconds()),
        object: "chat.completion.chunk",
        created: now_seconds(),
        model: client_model.to_string(),
        choices: vec![ChatCompletionChunkChoice {
            index: 0,
            delta: ChatCompletionDelta {
                role: None,
                content: Some(content),
            },
            finish_reason: None,
        }],
    }
}

pub fn final_stream_chunk(client_model: &str) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: format!("chatcmpl-{}", now_seconds()),
        object: "chat.completion.chunk",
        created: now_seconds(),
        model: client_model.to_string(),
        choices: vec![ChatCompletionChunkChoice {
            index: 0,
            delta: ChatCompletionDelta {
                role: None,
                content: None,
            },
            finish_reason: Some("stop".to_string()),
        }],
    }
}

impl ChatCompletionRequest {
    pub fn to_codex_request(&self, settings: &ProxySettings) -> Result<CodexRequest, ProxyError> {
        let upstream_model = if self.model == "gpt-5.5-fast" {
            "gpt-5.5"
        } else {
            self.model.as_str()
        };
        let upstream_service_tier = match self.service_tier.as_deref() {
            Some("none") => None,
            Some("fast") | Some("priority") if settings.priority_service => {
                Some("priority".to_string())
            }
            Some("fast") | Some("priority") => None,
            Some(service_tier) => Some(service_tier.to_string()),
            None if self.model == "gpt-5.5-fast" && settings.priority_service => {
                Some("priority".to_string())
            }
            None if matches!(settings.speed, Speed::Fast) && settings.priority_service => {
                Some("priority".to_string())
            }
            None => None,
        };
        let reasoning_effort = self
            .reasoning_effort
            .unwrap_or(settings.reasoning_effort)
            .as_upstream_value()
            .to_string();

        Ok(CodexRequest {
            model: upstream_model.to_string(),
            instructions: codex_instructions_from_messages(&self.messages),
            input: self
                .messages
                .iter()
                .filter(|message| message.role != "system")
                .map(chat_message_to_codex_input)
                .collect::<Result<Vec<_>, _>>()?,
            reasoning: CodexReasoning {
                effort: reasoning_effort,
            },
            stream: true,
            store: false,
            service_tier: upstream_service_tier,
            prompt_cache_key: None,
        })
    }
}

fn codex_instructions_from_messages(messages: &[ChatMessage]) -> String {
    let system_text = messages
        .iter()
        .filter(|message| message.role == "system")
        .filter_map(chat_message_text)
        .collect::<Vec<_>>()
        .join("\n\n");

    if system_text.is_empty() {
        return "You are Codex.".to_string();
    }

    system_text
}

fn chat_message_text(chat_message: &ChatMessage) -> Option<String> {
    match &chat_message.content {
        ChatMessageContent::Text(text) => Some(text.clone()),
        ChatMessageContent::Parts(parts) => {
            let text = parts
                .iter()
                .filter_map(|part| match part {
                    ChatMessageContentPart::Text { text } => Some(text.as_str()),
                    ChatMessageContentPart::ImageUrl { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n");

            if text.is_empty() { None } else { Some(text) }
        }
    }
}

fn chat_message_to_codex_input(chat_message: &ChatMessage) -> Result<Value, ProxyError> {
    Ok(serde_json::json!({
        "type": "message",
        "role": chat_message.role,
        "content": match &chat_message.content {
            ChatMessageContent::Text(text) => vec![serde_json::json!({
                "type": "input_text",
                "text": text,
            })],
            ChatMessageContent::Parts(parts) => parts
                .iter()
                .map(chat_message_part_to_codex_content)
                .collect::<Result<Vec<_>, _>>()?,
        },
    }))
}

fn chat_message_part_to_codex_content(part: &ChatMessageContentPart) -> Result<Value, ProxyError> {
    match part {
        ChatMessageContentPart::Text { text } => Ok(serde_json::json!({
            "type": "input_text",
            "text": text,
        })),
        ChatMessageContentPart::ImageUrl { image_url } => {
            if image_url.url.is_empty() {
                return Err(ProxyError::InvalidRequest(
                    "image_url.url must not be empty".to_string(),
                ));
            }
            Ok(serde_json::json!({
                "type": "input_image",
                "image_url": image_url.url,
            }))
        }
    }
}

fn model_response(id: &'static str) -> ModelResponse {
    ModelResponse {
        id,
        object: "model",
        created: 0,
        owned_by: "codex-proxy",
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ReasoningEffort, Speed};

    #[test]
    fn models_response_contains_only_public_models() {
        let models_response = models_response();
        let model_ids = models_response
            .data
            .into_iter()
            .map(|model| model.id)
            .collect::<Vec<_>>();

        assert_eq!(model_ids, vec!["gpt-5.5", "gpt-5.5-fast"]);
    }

    #[test]
    fn fast_model_maps_to_priority_service_tier() {
        let upstream_request = ChatCompletionRequest {
            model: "gpt-5.5-fast".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: ChatMessageContent::Text("hello".to_string()),
            }],
            stream: false,
            service_tier: None,
            reasoning_effort: None,
        }
        .to_codex_request(&ProxySettings::default())
        .expect("request should convert");

        assert_eq!(upstream_request.model, "gpt-5.5");
        assert_eq!(upstream_request.service_tier.as_deref(), Some("priority"));
        assert_eq!(upstream_request.reasoning.effort, "medium");
        assert!(upstream_request.stream);
        assert!(upstream_request.prompt_cache_key.is_none());
    }

    #[test]
    fn non_streaming_client_request_still_streams_upstream() {
        let upstream_request = ChatCompletionRequest {
            model: "gpt-5.5".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: ChatMessageContent::Text("hello".to_string()),
            }],
            stream: false,
            service_tier: None,
            reasoning_effort: None,
        }
        .to_codex_request(&ProxySettings::default())
        .expect("request should convert");

        assert!(upstream_request.stream);
    }

    #[test]
    fn fast_service_tier_maps_to_priority() {
        let upstream_request = ChatCompletionRequest {
            model: "gpt-5.5".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: ChatMessageContent::Text("hello".to_string()),
            }],
            stream: false,
            service_tier: Some("fast".to_string()),
            reasoning_effort: None,
        }
        .to_codex_request(&ProxySettings::default())
        .expect("request should convert");

        assert_eq!(upstream_request.service_tier.as_deref(), Some("priority"));
    }

    #[test]
    fn text_and_image_parts_convert_to_codex_input() {
        let upstream_request = ChatCompletionRequest {
            model: "gpt-5.5".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: ChatMessageContent::Parts(vec![
                    ChatMessageContentPart::Text {
                        text: "what is this?".to_string(),
                    },
                    ChatMessageContentPart::ImageUrl {
                        image_url: ImageUrlPart {
                            url: "data:image/png;base64,abc".to_string(),
                        },
                    },
                ]),
            }],
            stream: false,
            service_tier: None,
            reasoning_effort: None,
        }
        .to_codex_request(&ProxySettings::default())
        .expect("request should convert");

        assert_eq!(
            upstream_request.input[0]["content"][0],
            serde_json::json!({"type": "input_text", "text": "what is this?"})
        );
        assert_eq!(
            upstream_request.input[0]["content"][1],
            serde_json::json!({"type": "input_image", "image_url": "data:image/png;base64,abc"})
        );
    }

    #[test]
    fn system_role_messages_become_instructions_not_input() {
        let upstream_request = ChatCompletionRequest {
            model: "gpt-5.5".to_string(),
            messages: vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: ChatMessageContent::Text("answer in Lithuanian".to_string()),
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: ChatMessageContent::Text("hello".to_string()),
                },
            ],
            stream: false,
            service_tier: None,
            reasoning_effort: None,
        }
        .to_codex_request(&ProxySettings::default())
        .expect("request should convert");

        assert_eq!(upstream_request.instructions, "answer in Lithuanian");
        assert_eq!(upstream_request.input.len(), 1);
        assert_eq!(upstream_request.input[0]["role"], "user");
    }

    #[test]
    fn system_part_texts_become_joined_instructions() {
        let upstream_request = ChatCompletionRequest {
            model: "gpt-5.5".to_string(),
            messages: vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: ChatMessageContent::Parts(vec![
                        ChatMessageContentPart::Text {
                            text: "first rule".to_string(),
                        },
                        ChatMessageContentPart::ImageUrl {
                            image_url: ImageUrlPart {
                                url: "data:image/png;base64,abc".to_string(),
                            },
                        },
                        ChatMessageContentPart::Text {
                            text: "second rule".to_string(),
                        },
                    ]),
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: ChatMessageContent::Text("hello".to_string()),
                },
            ],
            stream: false,
            service_tier: None,
            reasoning_effort: None,
        }
        .to_codex_request(&ProxySettings::default())
        .expect("request should convert");

        assert_eq!(upstream_request.instructions, "first rule\nsecond rule");
        assert_eq!(upstream_request.input.len(), 1);
    }

    #[test]
    fn settings_control_reasoning_and_speed_defaults() {
        let upstream_request = ChatCompletionRequest {
            model: "gpt-5.5".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: ChatMessageContent::Text("hello".to_string()),
            }],
            stream: false,
            service_tier: None,
            reasoning_effort: None,
        }
        .to_codex_request(&ProxySettings {
            reasoning_effort: ReasoningEffort::High,
            speed: Speed::Fast,
            ..ProxySettings::default()
        })
        .expect("request should convert");

        assert_eq!(upstream_request.reasoning.effort, "high");
        assert_eq!(upstream_request.service_tier.as_deref(), Some("priority"));
    }

    #[test]
    fn request_reasoning_effort_overrides_settings_default() {
        let upstream_request = ChatCompletionRequest {
            model: "gpt-5.5".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: ChatMessageContent::Text("hello".to_string()),
            }],
            stream: false,
            service_tier: None,
            reasoning_effort: Some(ReasoningEffort::Low),
        }
        .to_codex_request(&ProxySettings {
            reasoning_effort: ReasoningEffort::High,
            ..ProxySettings::default()
        })
        .expect("request should convert");

        assert_eq!(upstream_request.reasoning.effort, "low");
    }

    #[test]
    fn priority_service_can_be_disabled() {
        let upstream_request = ChatCompletionRequest {
            model: "gpt-5.5-fast".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: ChatMessageContent::Text("hello".to_string()),
            }],
            stream: false,
            service_tier: Some("fast".to_string()),
            reasoning_effort: None,
        }
        .to_codex_request(&ProxySettings {
            speed: Speed::Fast,
            priority_service: false,
            ..ProxySettings::default()
        })
        .expect("request should convert");

        assert_eq!(upstream_request.model, "gpt-5.5");
        assert!(upstream_request.service_tier.is_none());
    }

    #[test]
    fn default_settings_keep_detailed_logs_off() {
        assert!(!ProxySettings::default().detailed_logs);
    }
}
