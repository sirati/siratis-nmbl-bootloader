#![forbid(unsafe_code)]

pub mod manifest;
pub mod prepare;
pub mod protocol;
pub mod transaction;
pub mod validate;

use std::{io, path::PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
    #[error("signature verification failed: {0}")]
    Signature(String),
}

impl Error {
    pub fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub struct Request {
    pub bundle: PathBuf,
}
