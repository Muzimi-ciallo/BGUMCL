use serde::Serialize;
use std::error::Error;

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct BGUMCLError(pub String);

pub type BGUMCLResult<T> = Result<T, BGUMCLError>;

impl<T> From<T> for BGUMCLError
where
  T: Error,
{
  fn from(err: T) -> Self {
    BGUMCLError(err.to_string())
  }
}
