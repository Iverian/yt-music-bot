#![allow(clippy::module_name_repetitions)]

use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot};

pub type ChannelResult<T> = core::result::Result<T, ChannelError>;

#[derive(Debug, Error, Clone, Copy)]
#[error("channel error")]
pub struct ChannelError;

impl From<oneshot::error::RecvError> for ChannelError {
    fn from(_value: oneshot::error::RecvError) -> Self {
        Self
    }
}

impl<T> From<mpsc::error::SendError<T>> for ChannelError {
    fn from(_value: mpsc::error::SendError<T>) -> Self {
        Self
    }
}

impl<T> From<broadcast::error::SendError<T>> for ChannelError {
    fn from(_value: broadcast::error::SendError<T>) -> Self {
        ChannelError
    }
}
