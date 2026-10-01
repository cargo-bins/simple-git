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

pub type GitUrlParseError = GitError;
