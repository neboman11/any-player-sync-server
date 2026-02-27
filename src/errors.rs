use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    pub fn bad_request(message: String) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "bad_request",
            message,
        }
    }

    pub fn conflict(message: String) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code: "version_conflict",
            message,
        }
    }

    pub fn internal(message: String) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_error",
            message,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let payload = json!({
            "error": {
                "code": self.code,
                "message": self.message,
            }
        });

        (self.status, Json(payload)).into_response()
    }
}
