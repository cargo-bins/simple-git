use std::{fmt, mem, num::NonZeroU32, path::Path, str::FromStr, sync::atomic::AtomicBool};

use gix::{
    clone, create,
    open::{self, Permissions},
    remote, Url,
};
use tracing::debug;

mod progress_tracing;
use progress_tracing::TracingProgress;

mod cancellation_token;
pub use cancellation_token::{GitCancelOnDrop, GitCancellationToken};

mod error;
pub use error::{GitError, GitUrlParseError};

#[derive(Clone, Debug)]
pub struct GitUrl(Url);

impl fmt::Display for GitUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl FromStr for GitUrl {
    type Err = GitUrlParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Url::try_from(s).map(Self).map_err(GitError::new)
    }
}

#[derive(Debug)]
pub struct Repository(gix::ThreadSafeRepository);

impl Repository {
    fn prepare_fetch(
        url: GitUrl,
        path: &Path,
        kind: create::Kind,
    ) -> Result<clone::PrepareFetch, GitError> {
        Ok(clone::PrepareFetch::new(
            url.0,
            path,
            kind,
            create::Options {
                destination_must_be_empty: Some(true),
                ..Default::default()
            },
            open::Options::default().permissions(Permissions::all()),
        )
        .map_err(GitError::new)?
        .with_shallow(remote::fetch::Shallow::DepthAtRemote(
            NonZeroU32::new(1).unwrap(),
        )))
    }

    /// WARNING: This is a blocking operation, if you want to use it in
    /// async context then you must wrap the call in [`tokio::task::spawn_blocking`].
    ///
    /// WARNING: This function must be called after tokio runtime is initialized.
    pub fn shallow_clone_bare(
        url: GitUrl,
        path: &Path,
        cancellation_token: Option<GitCancellationToken>,
    ) -> Result<Self, GitError> {
        debug!("Shallow cloning {url} to {}", path.display());

        Ok(Self(
            Self::prepare_fetch(url, path, create::Kind::Bare)?
                .fetch_only(
                    &mut TracingProgress::new("Cloning bare"),
                    cancellation_token
                        .as_ref()
                        .map(GitCancellationToken::get_atomic)
                        .unwrap_or(&AtomicBool::new(false)),
                )
                .map_err(GitError::new)?
                .0
                .into(),
        ))
    }

    /// WARNING: This is a blocking operation, if you want to use it in
    /// async context then you must wrap the call in [`tokio::task::spawn_blocking`].
    ///
    /// WARNING: This function must be called after tokio runtime is initialized.
    pub fn shallow_clone(
        url: GitUrl,
        path: &Path,
        cancellation_token: Option<GitCancellationToken>,
    ) -> Result<Self, GitError> {
        debug!("Shallow cloning {url} to {} with worktree", path.display());

        let mut progress = TracingProgress::new("Cloning with worktree");

        let should_interrupt = match &cancellation_token {
            Some(token) => token.get_atomic(),
            None => &AtomicBool::new(false),
        };

        Ok(Self(
            Self::prepare_fetch(url, path, create::Kind::WithWorktree)?
                .fetch_then_checkout(&mut progress, should_interrupt)
                .map_err(GitError::new)?
                .0
                .main_worktree(&mut progress, should_interrupt)
                .map_err(GitError::new)?
                .0
                .into(),
        ))
    }

    /// Open an existing repository at `path`, e.g. one created by
    /// [`Repository::shallow_clone_bare`] in an earlier run.
    ///
    /// WARNING: This is a blocking operation, if you want to use it in
    /// async context then you must wrap the call in [`tokio::task::spawn_blocking`].
    pub fn open(path: &Path) -> Result<Self, GitError> {
        debug!("Opening git repository at {}", path.display());

        gix::open_opts(
            path,
            open::Options::default().permissions(Permissions::all()),
        )
        .map(|repo| Self(repo.into()))
        .map_err(GitError::new)
    }

    /// Fetch the commit `HEAD` of `url` points to with depth 1, then
    /// point the local `HEAD` at it (detached).
    ///
    /// This updates a repository created by [`Repository::shallow_clone_bare`]
    /// (or opened with [`Repository::open`]) in place: the remote only sends
    /// the objects the local repository does not have yet, so it is usually
    /// much cheaper than cloning again.
    ///
    /// Returns `true` if a pack was received, `false` if the local repository
    /// already had everything.
    ///
    /// Objects that are no longer reachable are not removed, so the
    /// repository grows with every fetch that receives a pack.
    ///
    /// WARNING: This is a blocking operation, if you want to use it in
    /// async context then you must wrap the call in [`tokio::task::spawn_blocking`].
    ///
    /// WARNING: This function must be called after tokio runtime is initialized.
    pub fn shallow_fetch_head(
        &self,
        url: GitUrl,
        cancellation_token: Option<GitCancellationToken>,
    ) -> Result<bool, GitError> {
        /// The ref fetched `HEAD` is written to, so that its commit is
        /// referenced by a ref, as git expects.
        const FETCHED_HEAD: &str = "refs/remotes/origin/HEAD";

        fn inner(
            this: &Repository,
            url: Url,
            should_interrupt: &AtomicBool,
        ) -> Result<bool, gix::Error> {
            let repo = this.0.to_thread_local();
            let mut progress = TracingProgress::new("Fetching");

            let prepare = repo
                .remote_at(url)?
                .with_refspecs(
                    Some(format!("+HEAD:{FETCHED_HEAD}").as_str()),
                    remote::Direction::Fetch,
                )?
                .with_fetch_tags(remote::fetch::Tags::None)
                .connect(remote::Direction::Fetch)?
                .prepare_fetch(&mut progress, Default::default())?;

            // A shallow fetch always negotiates and receives a pack, even
            // if the remote `HEAD` has not moved, so check that first.
            let remote_head = prepare
                .ref_map()
                .mappings
                .iter()
                .find_map(|mapping| mapping.remote.as_id());
            if let (Some(remote_head), Ok(local_head)) = (remote_head, repo.head_id()) {
                if remote_head == local_head.detach() {
                    debug!("HEAD is up to date");
                    return Ok(false);
                }
            }

            let outcome = prepare
                .with_shallow(remote::fetch::Shallow::DepthAtRemote(
                    NonZeroU32::new(1).unwrap(),
                ))
                .receive(&mut progress, should_interrupt)?;

            let received_pack = matches!(outcome.status, remote::fetch::Status::Change { .. });
            debug!(received_pack, "Fetched HEAD");

            let fetched = repo.find_reference(FETCHED_HEAD)?.into_fully_peeled_id()?;

            // Without dereferencing, so `HEAD` becomes detached instead of
            // updating the branch it points to.
            repo.reference(
                "HEAD",
                fetched,
                gix::refs::transaction::PreviousValue::Any,
                "fetch: point HEAD at the fetched remote HEAD",
            )?;

            Ok(received_pack)
        }

        debug!("Shallow fetching HEAD of {url}");

        inner(
            self,
            url.0,
            cancellation_token
                .as_ref()
                .map(GitCancellationToken::get_atomic)
                .unwrap_or(&AtomicBool::new(false)),
        )
        .map_err(GitError::new)
    }

    /// Path to the repository's git directory, e.g. the repository itself for
    /// a bare one.
    pub fn git_dir(&self) -> &Path {
        self.0.git_dir()
    }

    #[inline(always)]
    pub fn get_head_commit_entry_data_by_path(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<Option<Vec<u8>>, GitError> {
        fn inner(this: &Repository, path: &Path) -> Result<Option<Vec<u8>>, gix::Error> {
            Ok(
                if let Some(entry) = this
                    .0
                    .to_thread_local()
                    .head_commit()?
                    .tree()?
                    .peel_to_entry_by_path(path)?
                {
                    Some(mem::take(&mut entry.object()?.data))
                } else {
                    None
                },
            )
        }

        inner(self, path.as_ref()).map_err(GitError::new)
    }

    pub fn get_head_commit_hash(&self) -> Result<impl fmt::Display, GitError> {
        Ok(self
            .0
            .to_thread_local()
            .head_commit()
            .map_err(GitError::new)?
            .id)
    }
}
