//! Rust-only acceptance support checks. These need neither a TestContext nor
//! a running Tauri application, so `cargo test -p slashit-acceptance` runs
//! them with the ordinary harness tests.

#![cfg(all(target_os = "linux", not(feature = "run-acceptance")))]
#![allow(dead_code)]

#[path = "support/git_fixture.rs"]
#[allow(dead_code)]
mod git_fixture;
#[path = "support/git_support.rs"]
mod git_support;
#[path = "support/task_assertions.rs"]
mod task_assertions;

use anyhow::Result;
use git_fixture::GitFixture;
use git_support::{git_commit_exists, git_is_ancestor, git_out};

/// The answers Git predicates give are Git's own yes and no; a Git failure is
/// an error, never a "no" an assertion could pass on.
#[test]
fn git_predicates_answer_yes_and_no_and_report_git_failing() -> Result<()> {
    let parent = std::env::temp_dir().join(format!(
        "slashit-git-predicates-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&parent).expect("create the scratch parent");
    let repo = parent.join("fixture-repo");
    GitFixture::create(&repo).expect("create the fixture");

    let base = git_out(&repo, &["rev-parse", "HEAD"]).expect("the base commit");
    let identity = ["-c", "user.name=t", "-c", "user.email=t@example.com"];
    let commit = |message: &str| {
        let mut args = identity.to_vec();
        args.extend(["commit", "--quiet", "--allow-empty", "-m", message]);
        git_out(&repo, &args).expect("commit");
        git_out(&repo, &["rev-parse", "HEAD"]).expect("the new commit")
    };
    let tip = commit("later");
    let missing = "0123456789012345678901234567890123456789";

    assert!(git_is_ancestor(&repo, &base, &tip)?);
    assert!(!git_is_ancestor(&repo, &tip, &base)?);
    assert!(git_commit_exists(&repo, &tip)?);
    assert!(!git_commit_exists(&repo, missing)?);

    let unknown = git_is_ancestor(&repo, &base, missing)
        .unwrap_err()
        .to_string();
    assert!(
        unknown.contains("failed unexpectedly") && unknown.contains("Not a valid commit name"),
        "{unknown}"
    );
    let not_a_repository = parent.join("not-a-repository");
    std::fs::create_dir_all(&not_a_repository).expect("create a plain directory");
    // An invalid .git marker stops Git discovering the checkout above this
    // temporary path and makes repository failures explicit.
    std::fs::write(not_a_repository.join(".git"), "not a gitfile\n").expect("write the marker");
    assert!(git_is_ancestor(&not_a_repository, &base, &tip).is_err());
    assert!(git_commit_exists(&not_a_repository, &tip).is_err());

    std::fs::remove_dir_all(&parent).expect("clean up");
    Ok(())
}
