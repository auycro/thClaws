//! OpenAI-style error envelope. Same shape OpenAI's clients parse:
//!
//! ```json
//! { "error": { "message": "...", "type": "...", "code": "...", "param": null } }
//! ```

use serde::Serialize;

#[derive(Serialize)]
pub struct OpenAiError {
    pub error: OpenAiErrorBody,
}

#[derive(Serialize)]
pub struct OpenAiErrorBody {
    pub message: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub code: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
}

impl OpenAiError {
    pub fn invalid_api_key() -> Self {
        Self {
            error: OpenAiErrorBody {
                message: "Invalid API key. Set THCLAWS_API_TOKEN on the server, then send it as `Authorization: Bearer <token>`.".into(),
                kind: "invalid_request_error",
                code: "invalid_api_key",
                param: None,
            },
        }
    }

    pub fn invalid_request(message: impl Into<String>, code: &'static str) -> Self {
        Self {
            error: OpenAiErrorBody {
                message: message.into(),
                kind: "invalid_request_error",
                code,
                param: None,
            },
        }
    }

    pub fn server_error(message: impl Into<String>) -> Self {
        Self {
            error: OpenAiErrorBody {
                message: message.into(),
                kind: "server_error",
                code: "internal_error",
                param: None,
            },
        }
    }
}
