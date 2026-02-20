// Minimal stub for workspace compilation - full implementation in Session 2

use thiserror::Error;

#[derive(Error, Debug)]
pub enum AetherError {
    #[error("Generic error: {0}")]
    Generic(String),
}
