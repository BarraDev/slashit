//! The fixture repositories themselves, with no application involved.
//!
//! A journey that creates more than one [`GitFixture`] puts them all in one
//! state root. Each has to get a bare `origin` of its own there: two fixtures
//! sharing one would each push an unrelated `main` into it, and whether the
//! second push is rejected would depend on nothing more than whether the two
//! initial commits happened to be made in the same second.

use super::GitFixture;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// A private parent for one test, so tests running in parallel in this
/// process, or in another one, never look at each other's fixtures.
fn scratch(label: &str) -> PathBuf {
    let parent = std::env::temp_dir().join(format!(
        "slashit-git-fixture-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&parent);
    std::fs::create_dir_all(&parent).expect("create the scratch parent");
    parent
}

/// Run a read-only git query in `dir` and return its trimmed output.
fn query(dir: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .with_context(|| format!("could not run git {args:?}"))?;
    if !output.status.success() {
        bail!(
            "git {args:?} failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Where `fixture` pushes, and whether what is there is its own `main`.
///
/// Asked of git rather than of the fixture, so the answer is what the product
/// will actually see.
fn own_origin(fixture: &Path) -> Result<PathBuf> {
    let origin = PathBuf::from(query(fixture, &["remote", "get-url", "--push", "origin"])?);
    let local = query(fixture, &["rev-parse", "refs/heads/main"])?;
    let pushed = query(&origin, &["rev-parse", "refs/heads/main"])?;
    if local != pushed {
        bail!(
            "{} holds {pushed} as main, but {} has {local}",
            origin.display(),
            fixture.display()
        );
    }
    Ok(origin)
}

/// Fixtures created at once, in one state root, as fast as threads can go:
/// every one of them is created, and every one pushes to an origin no other
/// fixture shares.
#[test]
fn fixtures_created_together_in_one_state_root_each_get_their_own_origin() {
    const FIXTURES: usize = 8;
    let parent = scratch("together");

    let fixtures: Vec<PathBuf> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..FIXTURES)
            .map(|index| {
                let path = parent.join(format!("fixture-repo-{index}"));
                scope.spawn(move || {
                    GitFixture::create(&path).expect("create a fixture");
                    path
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("thread"))
            .collect()
    });

    let origins: std::collections::BTreeSet<PathBuf> = fixtures
        .iter()
        .map(|fixture| own_origin(fixture).expect("the fixture's own origin"))
        .collect();
    assert_eq!(
        origins.len(),
        FIXTURES,
        "fixtures in one state root were handed a shared origin: {origins:?}"
    );

    std::fs::remove_dir_all(&parent).expect("clean up");
}

/// The case hosted CI hit: a second fixture made after the clock has moved
/// on commits a `main` unrelated to the first one's, and must still be able
/// to push it.
#[test]
fn a_fixture_created_in_a_later_second_still_pushes_its_own_main() {
    let parent = scratch("later-second");

    let first = parent.join("fixture-repo");
    GitFixture::create(&first).expect("create the first fixture");
    // Commit timestamps have one-second resolution; past this, the second
    // fixture's initial commit cannot be byte-identical to the first's.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let second = parent.join("fixture-repo-elsewhere");
    GitFixture::create(&second).expect("create a second fixture beside the first");

    let first_origin = own_origin(&first).expect("the first fixture's own origin");
    let second_origin = own_origin(&second).expect("the second fixture's own origin");
    assert_ne!(first_origin, second_origin);

    std::fs::remove_dir_all(&parent).expect("clean up");
}

/// An origin already on disk belongs to whoever made it. A fixture that
/// would land on it again is refused, rather than reinitialising it and
/// pushing into somebody else's repository.
#[test]
fn a_fixture_never_takes_over_an_origin_that_already_exists() {
    let parent = scratch("reuse");

    let path = parent.join("fixture-repo");
    GitFixture::create(&path).expect("create the fixture");
    let origin = own_origin(&path).expect("the fixture's own origin");
    let before = query(&origin, &["rev-parse", "refs/heads/main"]).expect("origin main");

    // Only the working repository goes, so a new fixture at the same place
    // gets as far as its own origin before anything can stop it.
    std::fs::remove_dir_all(&path).expect("remove the working repository");
    // The refusal has to be the claim on the origin itself: failing anywhere
    // later, on a push the old origin happens to reject, would pass here
    // while still writing into a repository this fixture does not own.
    let refusal = match GitFixture::create(&path) {
        Ok(_) => panic!("a new fixture was handed the origin an earlier one left behind"),
        Err(error) => format!("{error:#}"),
    };
    assert!(
        refusal.contains(&format!("could not create {}", origin.display())),
        "refused for some other reason than the existing origin: {refusal}"
    );
    let after = query(&origin, &["rev-parse", "refs/heads/main"]).expect("origin main");
    assert_eq!(
        before, after,
        "the refused fixture still changed the origin"
    );

    std::fs::remove_dir_all(&parent).expect("clean up");
}
