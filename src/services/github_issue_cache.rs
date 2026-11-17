// SPDX-License-Identifier: MPL-2.0
//! Session memory for Issue browsing: the Issues last loaded for each
//! provider, repository and query, kept while the application runs so that
//! leaving the Issues page and coming back shows them at once.
//!
//! Nothing here is persisted. The Issue bodies a person browsed stay in
//! memory only. The page owns what is shown; this module only answers for a
//! key and decides whether a response may still be applied.

use std::cell::RefCell;
use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::models::github::GithubIssue;

/// The provider the page browses. Only GitHub exists today.
pub const PROVIDER_GITHUB: &str = "github";

/// The one query the backend answers: the default open listing.
pub const OPEN_QUERY: &str = "open";

/// Identifies one browsed listing. The repository is compared without regard
/// to case, as GitHub does.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CacheKey {
    provider: &'static str,
    repo: String,
    query: &'static str,
}

impl CacheKey {
    /// The key for the open Issues of `repo` (`owner/name`) on GitHub.
    pub fn github(repo: &str) -> Self {
        Self {
            provider: PROVIDER_GITHUB,
            repo: repo.trim().to_ascii_lowercase(),
            query: OPEN_QUERY,
        }
    }
}

/// One loaded listing and when GitHub last answered for it.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedIssues {
    pub issues: Vec<GithubIssue>,
    pub refreshed_at: DateTime<Utc>,
}

/// The listings loaded in this session, one per key.
#[derive(Default)]
pub struct IssueCache {
    entries: HashMap<CacheKey, CachedIssues>,
}

impl IssueCache {
    pub fn get(&self, key: &CacheKey) -> Option<CachedIssues> {
        self.entries.get(key).cloned()
    }

    pub fn put(&mut self, key: CacheKey, entry: CachedIssues) {
        self.entries.insert(key, entry);
    }
}

thread_local! {
    static SESSION: RefCell<IssueCache> = RefCell::new(IssueCache::default());
}

/// The listing last loaded for `key` in this session, if any.
pub fn cached(key: &CacheKey) -> Option<CachedIssues> {
    SESSION.with(|cache| cache.borrow().get(key))
}

/// Remember a listing loaded for `key`.
pub fn remember(key: CacheKey, entry: CachedIssues) {
    SESSION.with(|cache| cache.borrow_mut().put(key, entry));
}

/// Whether a response for `requested` may replace what the page shows. Only
/// the newest request of the page counts, and only while the page still asks
/// for the same listing. A response for another repository, or one that a
/// newer request has overtaken, is dropped.
pub fn may_apply(latest_request: u64, request: u64, current: Option<&CacheKey>, requested: &CacheKey) -> bool {
    request == latest_request && current == Some(requested)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(number: i32) -> GithubIssue {
        GithubIssue {
            number,
            title: format!("Issue {number}"),
            body: String::new(),
            state: "OPEN".into(),
            labels: Vec::new(),
            assignees: Vec::new(),
            comments: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn entry(numbers: &[i32]) -> CachedIssues {
        CachedIssues {
            issues: numbers.iter().map(|n| issue(*n)).collect(),
            refreshed_at: Utc::now(),
        }
    }

    #[test]
    fn keys_ignore_case_and_surrounding_space_but_not_the_repository() {
        assert_eq!(CacheKey::github(" Owner/Repo "), CacheKey::github("owner/repo"));
        assert_ne!(CacheKey::github("owner/repo"), CacheKey::github("owner/other"));
    }

    #[test]
    fn a_listing_is_kept_per_repository() {
        let mut cache = IssueCache::default();
        cache.put(CacheKey::github("a/one"), entry(&[1, 2]));
        cache.put(CacheKey::github("b/two"), entry(&[9]));
        assert_eq!(cache.get(&CacheKey::github("a/one")).unwrap().issues.len(), 2);
        assert_eq!(cache.get(&CacheKey::github("b/two")).unwrap().issues[0].number, 9);
        assert!(cache.get(&CacheKey::github("c/three")).is_none());
    }

    #[test]
    fn a_response_for_another_repository_is_not_applied() {
        let page = CacheKey::github("a/one");
        let late = CacheKey::github("b/two");
        // The page moved to b/two; a/one's answer (request 1) arrives last.
        assert!(!may_apply(2, 1, Some(&late), &page));
        assert!(may_apply(2, 2, Some(&late), &late));
    }

    #[test]
    fn an_overtaken_request_for_the_same_repository_is_not_applied() {
        let key = CacheKey::github("a/one");
        assert!(!may_apply(3, 2, Some(&key), &key));
        assert!(may_apply(3, 3, Some(&key), &key));
        assert!(!may_apply(3, 3, None, &key));
    }
}
