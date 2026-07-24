use thiserror::Error;

pub type RuntimeResult<T> = Result<T, RuntimeError>;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("{code}: {message}")]
    Diagnostic { code: String, message: String },
    #[error("runtime store error: {0}")]
    Store(String),
    #[error("runtime provider error: {0}")]
    Provider(String),
    #[error("runtime serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("runtime clock error: {0}")]
    Clock(String),
}

impl RuntimeError {
    pub fn diagnostic(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Diagnostic {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn code(&self) -> &str {
        match self {
            Self::Diagnostic { code, .. } => code,
            Self::Store(_) => "runtime_store_error",
            Self::Provider(_) => "action_provider_error",
            Self::Serialization(_) => "runtime_serialization_error",
            Self::Clock(_) => "runtime_clock_error",
        }
    }
}
