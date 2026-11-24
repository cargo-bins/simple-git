use std::{fmt, mem, num::NonZeroU32, path::Path, str::FromStr, sync::atomic::AtomicBool};

use compact_str::CompactString;
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

/// Which commit of a remote repository to check out, like the
/// `--branch`, `--tag` and `--rev` options of `cargo install --git`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum GitReference {
    /// The commit the remote `HEAD` points to, usually the tip of the
    /// default branch.
    #[default]
    DefaultBranch,
    /// The tip of a branch, e.g. `main`.
    Branch(CompactString),
    /// A tag, e.g. `v1.0.0`.
    Tag(CompactString),
    /// A full commit hash, a full ref name like `refs/pull/1/head`, or any
    /// other revision git understands, such as an abbreviated commit hash.
    ///
    /// Only full commit hashes and full ref names can be fetched on their
    /// own; anything else needs every branch and tag with their whole
    /// history, so it is much slower.
    Rev(CompactString),
}

impl fmt::Display for GitReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DefaultBranch => f.write_str("default branch"),
            Self::Branch(branch) => write!(f, "branch {branch}"),
            Self::Tag(tag) => write!(f, "tag {tag}"),
            Self::Rev(rev) => write!(f, "rev {rev}"),
        }
    }
}

/// How [`Repository::shallow_clone_reference`] fetches a [`GitReference`].
enum Fetch {
    /// Fetch the remote `HEAD` with depth 1.
    DefaultBranch,
    /// Fetch only this refspec source with depth 1 and detach `HEAD` onto it.
    Revision(String),
    /// Fetch every branch and tag with full history, then resolve the
    /// revision locally.
    Everything(CompactString),
}

impl Fetch {
    fn new(reference: &GitReference) -> Self {
        match reference {
            GitReference::DefaultBranch => Self::DefaultBranch,
            GitReference::Branch(branch) => Self::Revision(format!("refs/heads/{branch}")),
            GitReference::Tag(tag) => Self::Revision(format!("refs/tags/{tag}")),
            GitReference::Rev(rev)
                if rev.starts_with("refs/")
                    || rev == "HEAD"
                    || gix::ObjectId::from_hex(rev.as_bytes()).is_ok() =>
            {
                Self::Revision(rev.to_string())
            }
            GitReference::Rev(rev) => Self::Everything(rev.clone()),
        }
    }
}

/// Resolve `rev` to a commit, like `git rev-parse <rev>^{commit}` would
/// for a ref name or an abbreviated commit hash, in a repository fetched
/// with [`Fetch::Everything`].
///
/// `gix`'s rev-parse is behind its `revision` feature, which pulls in a lot
/// more than this needs.
fn resolve_rev(repo: &gix::Repository, rev: &str) -> Result<gix::ObjectId, gix::Error> {
    use gix::error::{message, ResultExt};

    // The order git looks up a short ref name in, see gitrevisions(7),
    // with branches under `refs/remotes/origin/` as clone puts them there.
    for name in [
        format!("refs/tags/{rev}"),
        format!("refs/remotes/origin/{rev}"),
    ] {
        if let Some(mut reference) = repo.try_find_reference(name.as_str())? {
            return Ok(reference.peel_to_commit()?.id);
        }
    }

    if rev.len() >= 4 {
        if let Ok(prefix) = gix::hash::Prefix::from_hex(rev) {
            match repo.objects.lookup_prefix(prefix, None).or_error()? {
                Some(Ok(id)) => return Ok(repo.find_object(id)?.peel_to_commit()?.id),
                Some(Err(())) => {
                    return Err(message!("The abbreviated commit hash {rev} is ambiguous")
                        .validation_error())
                }
                None => {}
            }
        }
    }

    Err(message!("The remote has no branch, tag or commit matching {rev}").not_found_error())
}

#[derive(Debug)]
pub struct Repository(gix::ThreadSafeRepository);

impl Repository {
    fn prepare_fetch(
        url: GitUrl,
        path: &Path,
        kind: create::Kind,
    ) -> Result<clone::PrepareFetch, gix::Error> {
        Ok(clone::PrepareFetch::new(
            url.0,
            path,
            kind,
            create::Options {
                destination_must_be_empty: Some(true),
                ..Default::default()
            },
            open::Options::default().permissions(Permissions::all()),
        )?
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
            Self::prepare_fetch(url, path, create::Kind::Bare)
                .map_err(GitError::new)?
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
        Self::shallow_clone_reference(url, path, &GitReference::DefaultBranch, cancellation_token)
    }

    /// Clone `url` to `path` with a worktree checked out at `reference`.
    ///
    /// The clone is shallow (depth 1) unless `reference` is a
    /// [`GitReference::Rev`] that is neither a full commit hash nor a full
    /// ref name, e.g. an abbreviated commit hash: that needs the whole
    /// history of every branch and tag to be resolved.
    ///
    /// For anything but [`GitReference::DefaultBranch`], `HEAD` is detached.
    ///
    /// WARNING: This is a blocking operation, if you want to use it in
    /// async context then you must wrap the call in [`tokio::task::spawn_blocking`].
    ///
    /// WARNING: This function must be called after tokio runtime is initialized.
    pub fn shallow_clone_reference(
        url: GitUrl,
        path: &Path,
        reference: &GitReference,
        cancellation_token: Option<GitCancellationToken>,
    ) -> Result<Self, GitError> {
        fn inner(
            url: GitUrl,
            path: &Path,
            fetch: Fetch,
            should_interrupt: &AtomicBool,
        ) -> Result<gix::Repository, gix::Error> {
            let mut progress = TracingProgress::new("Cloning with worktree");

            let mut prepare = Repository::prepare_fetch(url, path, create::Kind::WithWorktree)?;
            match &fetch {
                Fetch::DefaultBranch => {}
                Fetch::Revision(revision) => {
                    prepare = prepare.with_revision(Some(revision.as_str()))?;
                }
                Fetch::Everything(_) => {
                    prepare = prepare.with_shallow(remote::fetch::Shallow::NoChange);
                }
            }

            let mut checkout = prepare
                .fetch_then_checkout(&mut progress, should_interrupt)?
                .0;

            if let Fetch::Everything(rev) = &fetch {
                let repo = checkout.repo();
                let commit = resolve_rev(repo, rev)?;
                // Without dereferencing, so `HEAD` becomes detached instead of
                // moving the branch it points to.
                repo.reference(
                    "HEAD",
                    commit,
                    gix::refs::transaction::PreviousValue::Any,
                    format!("checkout: moving to {rev}"),
                )?;
            }

            Ok(checkout.main_worktree(&mut progress, should_interrupt)?.0)
        }

        debug!(
            "Shallow cloning {url} at {reference} to {} with worktree",
            path.display()
        );

        let should_interrupt = match &cancellation_token {
            Some(token) => token.get_atomic(),
            None => &AtomicBool::new(false),
        };

        inner(url, path, Fetch::new(reference), should_interrupt)
            .map(|repo| Self(repo.into()))
            .map_err(GitError::new)
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
