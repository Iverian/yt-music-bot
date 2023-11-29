use std::error::Error as StdError;
use std::ops::Deref;
use std::str::FromStr;

use serde::Serialize;
use thiserror::Error;

use crate::player::MAX_VOLUME;

#[derive(Debug, Clone, Copy)]
pub enum VolumeCommand {
    Increase,
    Decrease,
    Set(u8),
}

#[derive(Debug, Clone, Copy, Error)]
#[error("invalid volume command")]
pub struct VolumeCommandError;

#[derive(Debug, Clone, Serialize)]
pub struct ArgumentList<T = String>(Vec<T>)
where
    T: Serialize;

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ArgumentListError<E>
where
    E: StdError + Send + Sync,
{
    #[error("invalid argument list: {0}")]
    Parse(String),
    #[error(transparent)]
    ParseArg(#[from] E),
}

impl<T> ArgumentList<T>
where
    T: Serialize,
{
    pub fn into_vec(self) -> Vec<T> {
        self.0
    }
}

impl<T> Default for ArgumentList<T>
where
    T: Serialize,
{
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T> Deref for ArgumentList<T>
where
    T: Serialize,
{
    type Target = Vec<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> From<ArgumentList<T>> for Vec<T>
where
    T: Serialize,
{
    fn from(value: ArgumentList<T>) -> Self {
        value.0
    }
}

impl<T> FromStr for ArgumentList<T>
where
    T: FromStr + Serialize,
    <T as FromStr>::Err: StdError + Send + Sync + 'static,
{
    type Err = ArgumentListError<<T as FromStr>::Err>;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Ok(Self::default());
        }

        shell_words::split(s)
            .map_err(|e| ArgumentListError::Parse(e.to_string()))?
            .into_iter()
            .map(|x| x.parse())
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
            .map(Self)
    }
}

impl FromStr for VolumeCommand {
    type Err = VolumeCommandError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "+" => VolumeCommand::Increase,
            "-" => VolumeCommand::Decrease,
            s => {
                let x: u8 = s.parse().map_err(|_| VolumeCommandError)?;
                if x > MAX_VOLUME {
                    return Err(VolumeCommandError);
                }
                VolumeCommand::Set(x)
            }
        })
    }
}
