use std::{fs, path::Path, process::Command};

use simple_git::{GitUrl, Repository};

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

fn head_file(repo: &Repository) -> Vec<u8> {
    repo.get_head_commit_entry_data_by_path("file")
        .unwrap()
        .expect("file exists")
}

#[tokio::test(flavor = "multi_thread")]
async fn shallow_fetch_head_updates_a_cached_clone() {
    let upstream = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let cache = cache.path().join("index");

    git(upstream.path(), &["init", "-q"]);
    commit(upstream.path(), "v1");

    let url: GitUrl = upstream.path().to_str().unwrap().parse().unwrap();

    let (url_clone, cache_clone) = (url.clone(), cache.clone());
    let repo = tokio::task::spawn_blocking(move || {
        Repository::shallow_clone_bare(url_clone, &cache_clone, None)
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(head_file(&repo), b"v1");
    drop(repo);

    let v2 = commit(upstream.path(), "v2");

    let (url_clone, cache_clone) = (url.clone(), cache.clone());
    let (repo, received_pack) = tokio::task::spawn_blocking(move || {
        let repo = Repository::open(&cache_clone)?;
        let received_pack = repo.shallow_fetch_head(url_clone, None)?;
        Ok::<_, simple_git::GitError>((repo, received_pack))
    })
    .await
    .unwrap()
    .unwrap();

    assert!(received_pack);
    assert_eq!(head_file(&repo), b"v2");
    assert_eq!(repo.get_head_commit_hash().unwrap().to_string(), v2);
    assert_eq!(repo.git_dir(), fs::canonicalize(&cache).unwrap());

    // Still shallow: only the fetched commit is present.
    let shallow = fs::read_to_string(cache.join("shallow")).unwrap();
    assert!(shallow.contains(&v2), "{shallow}");

    // Nothing new upstream.
    let received_pack = tokio::task::spawn_blocking(move || repo.shallow_fetch_head(url, None))
        .await
        .unwrap()
        .unwrap();
    assert!(!received_pack);
}
