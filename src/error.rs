use axum::Json;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error(
        "auth file was not found; run `codex-proxy login browser` or `codex-proxy login device`"
    )]
    MissingAuth,

    #[error("codex auth is expired and cannot be refreshed because no refresh token is saved")]
    MissingRefreshToken,

    #[error("failed to read auth file: {0}")]
    ReadAuth(std::io::Error),

    #[error("failed to write auth file: {0}")]
    WriteAuth(std::io::Error),

    #[error("failed to parse auth file: {0}")]
    ParseAuth(serde_json::Error),

    #[error("failed to read settings file: {0}")]
    ReadSettings(std::io::Error),

    #[error("failed to write settings file: {0}")]
    WriteSettings(std::io::Error),

    #[error("failed to parse settings file: {0}")]
    ParseSettings(serde_json::Error),

    #[error("failed to parse token response: {0}")]
    ParseToken(serde_json::Error),

    #[error("oauth request failed: {0}")]
    OAuthRequest(reqwest::Error),

    #[error("oauth request failed with status {status}: {body}")]
    OAuthStatus { status: u16, body: String },

    #[error("upstream request failed: {0}")]
    UpstreamRequest(reqwest::Error),

    #[error("upstream request failed with status {status}: {body}")]
    UpstreamStatus { status: u16, body: String },

    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("login failed: {0}")]
    Login(String),
}

impl ProxyError {
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::MissingAuth | Self::MissingRefreshToken => StatusCode::UNAUTHORIZED,
            Self::InvalidRequest(_) | Self::ParseAuth(_) | Self::ParseSettings(_) => {
                StatusCode::BAD_REQUEST
            }
            Self::OAuthStatus { status, .. } | Self::UpstreamStatus { status, .. } => {
                StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY)
            }
            Self::ReadAuth(_)
            | Self::WriteAuth(_)
            | Self::ReadSettings(_)
            | Self::WriteSettings(_)
            | Self::ParseToken(_)
            | Self::OAuthRequest(_)
            | Self::UpstreamRequest(_)
            | Self::Login(_) => StatusCode::BAD_GATEWAY,
        }
    }
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> axum::response::Response {
        let status_code = self.status_code();
        let error_response = OpenAiErrorResponse {
            error: OpenAiErrorBody {
                message: self.to_string(),
                error_type: "proxy_error",
                code: None,
            },
        };
        (status_code, Json(error_response)).into_response()
    }
}

#[derive(Serialize)]
struct OpenAiErrorResponse {
    error: OpenAiErrorBody,
}

#[derive(Serialize)]
struct OpenAiErrorBody {
    message: String,
    #[serde(rename = "type")]
    error_type: &'static str,
    code: Option<String>,
}
