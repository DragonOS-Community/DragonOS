use std::{error::Error, fmt::Display};

#[derive(Debug)]
pub enum BackendErrorKind {
    FileNotFound,
    KernelLoadError,
}

#[derive(Debug)]
pub struct BackendError {
    kind: BackendErrorKind,
    message: Option<String>,
}

impl BackendError {
    pub fn new(kind: BackendErrorKind, message: Option<String>) -> Self {
        Self { kind, message }
    }
}

impl Error for BackendError {}

impl Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            BackendErrorKind::FileNotFound => {
                write!(f, "File not found: {:?}", self.message.as_ref().unwrap())
            }
            BackendErrorKind::KernelLoadError => {
                write!(
                    f,
                    "Failed to load kernel: {:?}",
                    self.message.as_ref().unwrap()
                )
            }
        }
    }
}
