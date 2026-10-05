//! Mirrors of the backend's orphan scan (`worktree::orphans`).

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutPresence {
    Present,
    MissingRegistered,
    MissingLocked,
    Unverified,
}

impl CheckoutPresence {
    pub fn label(self) -> &'static str {
        match self {
            CheckoutPresence::Present => "Directory present",
            CheckoutPresence::MissingRegistered => "Directory gone, still registered by git",
            CheckoutPresence::MissingLocked => "Directory gone, registration locked",
            CheckoutPresence::Unverified => "Could not tell whether the directory is there",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OrphanCheckout {
    pub path: String,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub presence: CheckoutPresence,
    pub dirty: Option<bool>,
    pub refusal: Option<String>,
}

impl OrphanCheckout {
    /// What its uncommitted state is, in words; an unanswered question is
    /// never described as clean.
    pub fn work_label(&self) -> &'static str {
        match (self.presence, self.dirty) {
            (_, Some(true)) => "Has uncommitted changes",
            (_, Some(false)) => "No uncommitted changes",
            (CheckoutPresence::Present, None) => "Could not be inspected",
            (_, None) => "No directory to inspect",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OrphanBranch {
    pub name: String,
    pub tip: Option<String>,
    pub pushed: Option<bool>,
    pub durable_refs: Vec<String>,
    pub unique_commits: Option<u32>,
    pub checked_out_at: Option<String>,
    pub refusal: Option<String>,
}

impl OrphanBranch {
    /// Whether the commits survive elsewhere, which is what makes deletion
    /// safe. It says nothing about whether this branch itself was pushed.
    pub fn work_label(&self) -> String {
        match (self.durable_refs.is_empty(), self.unique_commits) {
            (false, _) => "Its commits are reachable from another ref".to_string(),
            (true, Some(n)) => format!("{n} commit(s) exist only on this branch"),
            (true, None) => "Could not establish whether its commits exist elsewhere".to_string(),
        }
    }

    /// Whether this branch itself has the expected remote evidence,
    /// independent of reachability.
    pub fn push_label(&self) -> &'static str {
        match self.pushed {
            Some(true) => "Pushed to the remote",
            Some(false) => "Not pushed to the remote",
            None => "Could not tell whether it was pushed",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct OrphanScan {
    pub checkouts: Vec<OrphanCheckout>,
    pub branches: Vec<OrphanBranch>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backend_scan_deserializes() {
        let scan: OrphanScan = serde_json::from_str(
            r#"{"checkouts":[{"path":"/w/task-1","branch":null,"head":"abc","presence":"missing_registered","dirty":null,"refusal":null}],
                "branches":[{"name":"task-1","tip":"abc","pushed":false,"durable_refs":[],"unique_commits":2,"checked_out_at":null,"refusal":"no"}]}"#,
        )
        .unwrap();
        assert_eq!(scan.checkouts[0].presence, CheckoutPresence::MissingRegistered);
        assert_eq!(scan.checkouts[0].work_label(), "No directory to inspect");
        assert_eq!(scan.branches[0].work_label(), "2 commit(s) exist only on this branch");
    }

    #[test]
    fn an_unanswered_question_is_never_labelled_clean() {
        let mut c = OrphanCheckout {
            path: "/w".into(),
            branch: None,
            head: None,
            presence: CheckoutPresence::Present,
            dirty: None,
            refusal: Some("x".into()),
        };
        assert_eq!(c.work_label(), "Could not be inspected");
        c.dirty = Some(true);
        assert_eq!(c.work_label(), "Has uncommitted changes");
        let b = OrphanBranch {
            name: "task-1".into(),
            tip: None,
            pushed: None,
            durable_refs: vec![],
            unique_commits: None,
            checked_out_at: None,
            refusal: Some("x".into()),
        };
        assert!(b.work_label().starts_with("Could not establish"));
    }

    #[test]
    fn push_state_is_separate_from_reachability() {
        let b = OrphanBranch {
            name: "task-1".into(),
            tip: Some("abc".into()),
            pushed: Some(false),
            durable_refs: vec!["refs/heads/main".into()],
            unique_commits: Some(0),
            checked_out_at: None,
            refusal: None,
        };
        assert_eq!(b.push_label(), "Not pushed to the remote");
        assert_eq!(b.work_label(), "Its commits are reachable from another ref");
        let pushed = OrphanBranch { pushed: Some(true), ..b.clone() };
        assert_eq!(pushed.push_label(), "Pushed to the remote");
        assert_eq!(pushed.work_label(), b.work_label());
        let unknown = OrphanBranch { pushed: None, ..b };
        assert!(unknown.push_label().starts_with("Could not tell"));
    }
}
