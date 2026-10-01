use std::panic;

use gix::Error as GixError;
use thiserror::Error as ThisError;

#[derive(Debug, ThisError)]
#[error(transparent)]
pub struct GitError(Box<GixError>);

impl GitError {
    pub(crate) fn new(err: GixError) -> Self {
        Self(Box::new(err))
    }
}

// GixError contains Box<dyn ...> without these bounds
// Error type doesn't have any state that can break anything
impl panic::UnwindSafe for GitError {}
impl panic::RefUnwindSafe for GitError {}

pub type GitUrlParseError = GitError;
