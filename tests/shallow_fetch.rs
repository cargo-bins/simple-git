use std::{fs, path::Path, process::Command};

use simple_git::{GitError, GitUrl, Repository};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn commit(dir: &Path, content: &str) -> String {
    fs::write(dir.join("file"), content).unwrap();
    git(dir, &["add", "file"]);
    git(dir, &["commit", "-q", "-m", content]);
    git(dir, &["rev-parse", "HEAD"]).trim().to_owned()
}

fn fetched_file(repo: &Repository) -> Vec<u8> {
    repo.get_entry_data_by_ref_and_path(Repository::FETCHED_HEAD, "file")
        .unwrap()
        .expect("file exists")
}

/// Open the repository at `path` like a new process would (creating it if
/// needed) and fetch `url`'s HEAD into it.
async fn open_and_fetch(url: &GitUrl, path: &Path, shallow: bool) -> (Repository, bool) {
    let (url, path) = (url.clone(), path.to_owned());
    tokio::task::spawn_blocking(move || {
        let repo = if path.exists() {
            Repository::open(&path)?
        } else {
            Repository::init(&path)?
        };
        let received_pack = repo.fetch_head(url, shallow, None)?;
        Ok::<_, GitError>((repo, received_pack))
    })
    .await
    .unwrap()
    .unwrap()
}

async fn fetch_updates_the_clone(shallow: bool) {
    let upstream = tempfile::tempdir().unwrap();
    let local = tempfile::tempdir().unwrap();
    let local = local.path().join("index");

    git(upstream.path(), &["init", "-q"]);
    let v1 = commit(upstream.path(), "v1");
    let url: GitUrl = upstream.path().to_str().unwrap().parse().unwrap();

    let (repo, received_pack) = open_and_fetch(&url, &local, shallow).await;
    assert!(received_pack);
    assert_eq!(fetched_file(&repo), b"v1");
    assert_eq!(repo.is_shallow().unwrap(), shallow);
    // Canonicalize both: temp dirs are behind symlinks on macOS and get a
    // `\\?\` prefix on Windows.
    assert_eq!(
        fs::canonicalize(repo.git_dir()).unwrap(),
        fs::canonicalize(local.join(".git")).unwrap()
    );
    drop(repo);

    // git sees the same thing.
    let fetched = git(&local, &["rev-parse", Repository::FETCHED_HEAD]);
    assert_eq!(fetched.trim(), v1);

    let v2 = commit(upstream.path(), "v2");
    let (repo, received_pack) = open_and_fetch(&url, &local, shallow).await;
    assert!(received_pack);
    assert_eq!(fetched_file(&repo), b"v2");
    assert_eq!(repo.is_shallow().unwrap(), shallow);
    drop(repo);

    let fetched = git(&local, &["rev-parse", Repository::FETCHED_HEAD]);
    assert_eq!(fetched.trim(), v2);
    let history = git(&local, &["rev-list", "--count", Repository::FETCHED_HEAD]);
    assert_eq!(history.trim(), if shallow { "1" } else { "2" });

    // Nothing new upstream.
    let (_, received_pack) = open_and_fetch(&url, &local, shallow).await;
    assert!(!received_pack);
}

#[tokio::test(flavor = "multi_thread")]
async fn shallow_fetch_updates_the_clone() {
    fetch_updates_the_clone(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn full_fetch_updates_the_clone() {
    fetch_updates_the_clone(false).await;
}
