// SPDX-License-Identifier: MPL-2.0
//! The durable record of PR-level review replies a Task has sent, or may have
//! sent, to GitHub.
//!
//! A review plan is replaced by every re-analysis, so a fact that must outlive
//! the plan cannot live in it. One record per reply is kept on the Task, keyed by
//! the canonical identity of the pull request it was addressed to and the
//! original comment it answers. A number alone does not identify a comment, and
//! a URL alone does not identify the history of a pull request: the same comment
//! number can exist on two pull requests, and a Task that is relinked keeps the
//! records of the pull request it was linked to before.
//!
//! The record answers two questions only: may this reply have reached GitHub and
//! is it held for a person, and did GitHub confirm it. Nothing else about the
//! reply is kept, and records are never removed automatically.

use serde::{Deserialize, Serialize};

use super::task::PrCommentKind;

/// The host of every pull request SlashIt publishes to. The URL parser accepts
/// only github.com, so a record needs no other host until it does.
pub const GITHUB_HOST: &str = "github.com";

/// The identity a record is kept under, borrowed from the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyKey<'a> {
    pub host: &'a str,
    /// `owner/name`, lowercased by the caller.
    pub repository: &'a str,
    pub pull_request: u64,
    pub comment_id: u64,
    /// The kind of GitHub resource `comment_id` names. Review and conversation
    /// comments have separate numbers, so a number alone is not an identity.
    pub kind: PrCommentKind,
}

/// Where a reply stands on GitHub, as far as SlashIt knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrReplyState {
    /// A POST for this reply was started and its outcome is unknown, or it
    /// failed in a way that may still have posted. Held until a person accepts
    /// the duplicate risk.
    Uncertain,
    /// GitHub confirmed the reply. It is never posted again.
    Confirmed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrReplyRecord {
    pub host: String,
    pub repository: String,
    pub pull_request: u64,
    pub comment_id: u64,
    pub state: PrReplyState,
    /// GitHub's id for the reply, when a confirmed POST returned one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_id: Option<u64>,
    /// The kind of resource `comment_id` named when the record was written.
    /// `None` only for a record written before the kind was kept; see [`find`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<PrCommentKind>,
}

impl PrReplyRecord {
    /// The same pull request and number, whatever the kind.
    fn names(&self, key: &ReplyKey<'_>) -> bool {
        self.host == key.host
            && self.repository == key.repository
            && self.pull_request == key.pull_request
            && self.comment_id == key.comment_id
    }

    /// The same resource: the number, and a kind equal to the key's.
    fn is_exact(&self, key: &ReplyKey<'_>) -> bool {
        self.names(key) && self.kind.as_ref() == Some(&key.kind)
    }
}

/// The record for `key`, if one exists. A record for the same kind of resource
/// is preferred. A record with no kind cannot say which resource it answered,
/// so it is returned for any kind of its number, and [`settled_state`] holds it.
pub fn find<'a>(records: &'a [PrReplyRecord], key: &ReplyKey<'_>) -> Option<&'a PrReplyRecord> {
    records
        .iter()
        .find(|r| r.is_exact(key))
        .or_else(|| records.iter().find(|r| r.kind.is_none() && r.names(key)))
}

/// What a record settles for the reply it was found for. A record with no kind
/// may describe another resource with the same number, so it holds the reply
/// instead of confirming it: a held reply costs a look at GitHub, while a wrong
/// confirmation would silently drop a reply nobody posted.
pub fn settled_state(record: &PrReplyRecord) -> PrReplyState {
    match record.kind {
        Some(_) => record.state,
        None => PrReplyState::Uncertain,
    }
}

fn upsert(
    records: &mut Vec<PrReplyRecord>,
    key: &ReplyKey<'_>,
    state: PrReplyState,
    reply_id: Option<u64>,
) {
    let record = PrReplyRecord {
        host: key.host.to_string(),
        repository: key.repository.to_string(),
        pull_request: key.pull_request,
        comment_id: key.comment_id,
        state,
        reply_id,
        kind: Some(key.kind.clone()),
    };
    // Only the same resource is replaced. A kind-less record is never
    // overwritten here, so its hold survives for the other resource it may name.
    match records.iter_mut().find(|r| r.is_exact(key)) {
        Some(existing) => *existing = record,
        None => records.push(record),
    }
}

/// Before a POST that may reach GitHub. The record is written before the POST,
/// so a crash afterwards leaves the reply held.
pub fn record_attempt(records: &mut Vec<PrReplyRecord>, key: &ReplyKey<'_>) {
    upsert(records, key, PrReplyState::Uncertain, None);
}

/// After GitHub confirmed the POST.
pub fn record_confirmed(
    records: &mut Vec<PrReplyRecord>,
    key: &ReplyKey<'_>,
    reply_id: Option<u64>,
) {
    upsert(records, key, PrReplyState::Confirmed, reply_id);
}

/// Carries what a plan saved before records existed says about one reply.
///
/// Idempotent. An uncertain flag always wins over a confirmed record, because
/// holding a reply that was in fact confirmed costs one look at GitHub, while
/// the reverse would repost it. A posted flag creates a confirmed record only
/// when none exists, so it never overrides a record written by this code.
pub fn import_legacy(
    records: &mut Vec<PrReplyRecord>,
    key: &ReplyKey<'_>,
    uncertain: bool,
    posted: bool,
    reply_id: Option<u64>,
) {
    if uncertain {
        upsert(records, key, PrReplyState::Uncertain, None);
    } else if posted && find(records, key).is_none() {
        upsert(records, key, PrReplyState::Confirmed, reply_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(repository: &str, pull_request: u64, comment_id: u64) -> ReplyKey<'_> {
        ReplyKey {
            host: GITHUB_HOST,
            repository,
            pull_request,
            comment_id,
            kind: PrCommentKind::Review,
        }
    }

    fn conversation(repository: &str, pull_request: u64, comment_id: u64) -> ReplyKey<'_> {
        ReplyKey {
            kind: PrCommentKind::Conversation,
            ..key(repository, pull_request, comment_id)
        }
    }

    #[test]
    fn a_record_is_kept_per_pull_request_and_comment() {
        let mut records = Vec::new();
        record_attempt(&mut records, &key("testorg/testrepo", 405, 77));

        assert!(find(&records, &key("testorg/testrepo", 405, 77)).is_some());
        // The same comment number on another pull request is another reply.
        assert!(find(&records, &key("testorg/testrepo", 406, 77)).is_none());
        assert!(find(&records, &key("testorg/testrepo", 405, 78)).is_none());
        assert!(find(&records, &key("other/repo", 405, 77)).is_none());
    }

    #[test]
    fn a_record_is_kept_per_resource_kind() {
        // Review and conversation comments can share a number on one pull request.
        let mut records = Vec::new();
        record_confirmed(&mut records, &key("testorg/testrepo", 405, 77), Some(1));

        assert!(find(&records, &conversation("testorg/testrepo", 405, 77)).is_none());
        record_attempt(&mut records, &conversation("testorg/testrepo", 405, 77));
        assert_eq!(records.len(), 2);
        assert_eq!(
            find(&records, &key("testorg/testrepo", 405, 77)).map(|r| r.state),
            Some(PrReplyState::Confirmed)
        );
        assert_eq!(
            find(&records, &conversation("testorg/testrepo", 405, 77)).map(|r| r.state),
            Some(PrReplyState::Uncertain)
        );
    }

    #[test]
    fn a_legacy_record_without_kind_holds_every_kind_of_its_number() {
        // Written before the kind was kept: the stored JSON has no `kind` field.
        let legacy: PrReplyRecord = serde_json::from_str(
            r#"{"host":"github.com","repository":"testorg/testrepo","pull_request":405,
                "comment_id":77,"state":"confirmed","reply_id":9}"#,
        )
        .expect("a legacy record still loads");
        assert_eq!(legacy.kind, None);
        let records = vec![legacy];

        let found = find(&records, &key("testorg/testrepo", 405, 77)).expect("found");
        assert_eq!(settled_state(found), PrReplyState::Uncertain);
        let other = find(&records, &conversation("testorg/testrepo", 405, 77)).expect("found");
        assert_eq!(settled_state(other), PrReplyState::Uncertain);
    }

    #[test]
    fn a_kind_record_is_preferred_and_a_legacy_record_is_never_overwritten() {
        let mut records = vec![PrReplyRecord {
            host: GITHUB_HOST.into(),
            repository: "testorg/testrepo".into(),
            pull_request: 405,
            comment_id: 77,
            state: PrReplyState::Confirmed,
            reply_id: Some(9),
            kind: None,
        }];
        record_attempt(&mut records, &key("testorg/testrepo", 405, 77));

        assert_eq!(records.len(), 2, "the legacy record stays");
        assert_eq!(records[0].kind, None);
        let found = find(&records, &key("testorg/testrepo", 405, 77)).expect("found");
        assert_eq!(found.kind, Some(PrCommentKind::Review));
        assert_eq!(settled_state(found), PrReplyState::Uncertain);
    }

    #[test]
    fn a_confirmed_record_replaces_the_attempt_it_settles() {
        let mut records = Vec::new();
        let k = key("testorg/testrepo", 405, 77);
        record_attempt(&mut records, &k);
        record_confirmed(&mut records, &k, Some(4242));

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].state, PrReplyState::Confirmed);
        assert_eq!(records[0].reply_id, Some(4242));
    }

    #[test]
    fn a_legacy_posted_flag_never_overrides_an_existing_record() {
        let mut records = Vec::new();
        let k = key("testorg/testrepo", 405, 77);
        record_attempt(&mut records, &k);
        import_legacy(&mut records, &k, false, true, Some(9));
        assert_eq!(records[0].state, PrReplyState::Uncertain);

        let mut empty = Vec::new();
        import_legacy(&mut empty, &k, false, true, Some(9));
        assert_eq!(empty[0].state, PrReplyState::Confirmed);
        assert_eq!(empty[0].reply_id, Some(9));
    }

    #[test]
    fn a_legacy_uncertain_flag_holds_even_over_a_confirmed_record() {
        let mut records = Vec::new();
        let k = key("testorg/testrepo", 405, 77);
        record_confirmed(&mut records, &k, Some(9));
        import_legacy(&mut records, &k, true, false, None);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].state, PrReplyState::Uncertain);
    }

    #[test]
    fn importing_twice_changes_nothing() {
        let mut once = Vec::new();
        let k = key("testorg/testrepo", 405, 77);
        import_legacy(&mut once, &k, true, false, None);
        let mut twice = once.clone();
        import_legacy(&mut twice, &k, true, false, None);
        assert_eq!(once, twice);
    }
}
