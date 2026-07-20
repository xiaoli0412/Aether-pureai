use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// Relay 引擎错误类型
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("channel config invalid: {0}")]
    InvalidConfig(String),

    #[error("price discovery failed for channel {channel_id}: {source}")]
    PriceDiscoveryFailed {
        channel_id: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("no available channel for model {model_id}")]
    NoAvailableChannel { model_id: String },

    #[error("upstream request failed: {0}")]
    UpstreamFailure(String),

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("runtime state error: {0}")]
    RuntimeState(String),

    #[error("database-backed downstream group export is unavailable")]
    DatabaseBackedGroupExportUnavailable,

    #[error("reconciliation error: {0}")]
    Reconciliation(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl IntoResponse for RelayError {
    fn into_response(self) -> Response {
        let (status, message, code) = match &self {
            RelayError::InvalidConfig(_) => (StatusCode::BAD_REQUEST, self.to_string(), None),
            RelayError::NoAvailableChannel { .. } => {
                (StatusCode::SERVICE_UNAVAILABLE, self.to_string(), None)
            }
            RelayError::DatabaseBackedGroupExportUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                self.to_string(),
                Some("database_backed_group_export_unavailable"),
            ),
            RelayError::NotFound(_) => (StatusCode::NOT_FOUND, self.to_string(), None),
            RelayError::Database(_) | RelayError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal server error".to_string(),
                None,
            ),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string(), None),
        };

        let body = match code {
            Some(code) => serde_json::json!({
                "error": true,
                "code": code,
                "message": message,
            }),
            None => serde_json::json!({
                "error": true,
                "message": message,
            }),
        };

        (status, axum::Json(body)).into_response()
    }
}
