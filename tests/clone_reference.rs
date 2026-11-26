use std::{fs, path::Path, process::Command};

use simple_git::{GitReference, GitUrl, Repository};

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

struct Upstream {
    dir: tempfile::TempDir,
    /// Not the tip of any branch, only reachable from `main`.
    old: String,
    main: String,
    tagged: String,
    feature: String,
    pull: String,
}

/// ```text
/// old - tagged - main            (main, HEAD)
///          \- feature            (feature, tags v1 + light)
///          \- pull               (refs/pull/1/head only)
/// ```
fn upstream() -> Upstream {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();

    git(path, &["init", "-q", "-b", "main"]);
    let old = commit(path, "old");
    let tagged = commit(path, "tagged");
    git(path, &["tag", "-a", "-m", "annotated", "v1"]);
    git(path, &["tag", "light"]);

    git(path, &["checkout", "-q", "-b", "feature"]);
    let feature = commit(path, "feature");

    git(path, &["checkout", "-q", "--detach", &tagged]);
    let pull = commit(path, "pull");
    git(path, &["update-ref", "refs/pull/1/head", &pull]);

    git(path, &["checkout", "-q", "main"]);
    let main = commit(path, "main");

    Upstream {
        dir,
        old,
        main,
        tagged,
        feature,
        pull,
    }
}

fn clone(
    upstream: &Upstream,
    reference: GitReference<'_>,
) -> Result<(String, String, bool), String> {
    let url: GitUrl = upstream.dir.path().to_str().unwrap().parse().unwrap();
    let dest = tempfile::tempdir().unwrap();
    let repo = Repository::shallow_clone_reference(url, dest.path(), reference, None)
        .map_err(|err| err.to_string())?;

    let file = fs::read_to_string(dest.path().join("file")).unwrap();
    assert_eq!(
        repo.get_head_commit_entry_data_by_path("file")
            .unwrap()
            .unwrap(),
        file.as_bytes(),
        "worktree and HEAD disagree"
    );
    let shallow = dest.path().join(".git").join("shallow").exists();
    Ok((
        repo.get_head_commit_hash().unwrap().to_string(),
        file,
        shallow,
    ))
}

fn rev(rev: &str) -> GitReference<'_> {
    GitReference::Rev(rev)
}

#[test]
fn default_branch() {
    let upstream = upstream();
    let (head, file, shallow) = clone(&upstream, GitReference::DefaultBranch).unwrap();
    assert_eq!(
        (head.as_str(), file.as_str(), shallow),
        (upstream.main.as_str(), "main", true)
    );
}

#[test]
fn branch() {
    let upstream = upstream();
    let (head, file, shallow) = clone(&upstream, GitReference::Branch("feature")).unwrap();
    assert_eq!(
        (head.as_str(), file.as_str(), shallow),
        (upstream.feature.as_str(), "feature", true)
    );

    let err = clone(&upstream, GitReference::Branch("v1")).unwrap_err();
    assert!(err.contains("refs/heads/v1"), "{err}");
}

#[test]
fn tag() {
    let upstream = upstream();
    for tag in ["v1", "light"] {
        let (head, file, shallow) = clone(&upstream, GitReference::Tag(tag)).unwrap();
        assert_eq!(
            (head.as_str(), file.as_str(), shallow),
            (upstream.tagged.as_str(), "tagged", true),
            "{tag}"
        );
    }

    let err = clone(&upstream, GitReference::Tag("feature")).unwrap_err();
    assert!(err.contains("refs/tags/feature"), "{err}");
}

#[test]
fn rev_full_hash() {
    let upstream = upstream();
    let (head, file, shallow) = clone(&upstream, rev(&upstream.old)).unwrap();
    assert_eq!(
        (head.as_str(), file.as_str(), shallow),
        (upstream.old.as_str(), "old", true)
    );
}

#[test]
fn rev_full_ref() {
    let upstream = upstream();
    let (head, file, shallow) = clone(&upstream, rev("refs/pull/1/head")).unwrap();
    assert_eq!(
        (head.as_str(), file.as_str(), shallow),
        (upstream.pull.as_str(), "pull", true)
    );
}

#[test]
fn rev_resolved_locally() {
    let upstream = upstream();

    let (head, file, shallow) = clone(&upstream, rev(&upstream.old[..8])).unwrap();
    assert_eq!(
        (head.as_str(), file.as_str(), shallow),
        (upstream.old.as_str(), "old", false)
    );

    let (head, file, _) = clone(&upstream, rev("feature")).unwrap();
    assert_eq!(
        (head.as_str(), file.as_str()),
        (upstream.feature.as_str(), "feature")
    );

    let (head, file, _) = clone(&upstream, rev("v1")).unwrap();
    assert_eq!(
        (head.as_str(), file.as_str()),
        (upstream.tagged.as_str(), "tagged")
    );

    let err = clone(&upstream, rev("nope")).unwrap_err();
    assert!(err.contains("nope"), "{err}");
}
