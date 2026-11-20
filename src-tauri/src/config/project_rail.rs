// SPDX-License-Identifier: MPL-2.0
//! Which Projects the Project Rail shows, and in what order.
//!
//! This is navigation state. Removing a Project from the rail does not change
//! its identity, repository, Tasks, Conversation or history, and it does not
//! stop any work. Admission is a separate concern and is not changed here.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The `[project_rail]` section of `config.toml`. Absent from older files,
/// which load as an empty order and nothing hidden.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectRailConfig {
    /// Visible Projects, first to last. A Project that is not listed sorts
    /// after the listed ones, by creation time.
    pub order: Vec<Uuid>,
    /// Projects removed from the rail. They keep their identity and data and
    /// can be restored at any time.
    pub hidden: Vec<Uuid>,
}

impl ProjectRailConfig {
    /// Drops Project ids that no longer exist and repeated ids, keeping the
    /// first occurrence. A Project is never listed as both visible and hidden:
    /// it stays hidden.
    pub fn normalized(self, exists: impl Fn(&Uuid) -> bool) -> Self {
        let mut seen = HashSet::new();
        let hidden: Vec<Uuid> = self
            .hidden
            .into_iter()
            .filter(|id| exists(id) && seen.insert(*id))
            .collect();
        let order = self
            .order
            .into_iter()
            .filter(|id| exists(id) && !hidden.contains(id) && seen.insert(*id))
            .collect();
        Self { order, hidden }
    }
}

/// Whether a Task in this status is work the user still has to act on or
/// that is still running. Such a Project must not leave the rail, because its
/// Needs You count would disappear with it.
pub fn is_outstanding_work(status: &crate::domain::TaskStatus) -> bool {
    use crate::domain::TaskStatus::*;
    matches!(status, Queue | InProgress | AiReview | HumanReview | PrCreated | Error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TaskStatus;

    #[test]
    fn absent_section_loads_as_an_empty_rail() {
        let parsed: ProjectRailConfig = toml::from_str("").unwrap();
        assert_eq!(parsed, ProjectRailConfig::default());
    }

    #[test]
    fn normalize_drops_unknown_and_repeated_ids_and_hidden_wins() {
        let (a, b, gone) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let config = ProjectRailConfig {
            order: vec![b, a, b, gone, a],
            hidden: vec![a, a, gone],
        };
        let known = |id: &Uuid| *id != gone;
        let normalized = config.normalized(known);
        assert_eq!(normalized.hidden, vec![a]);
        assert_eq!(normalized.order, vec![b]);
    }

    #[test]
    fn only_work_that_needs_the_user_or_is_running_blocks_removal() {
        assert!(is_outstanding_work(&TaskStatus::Queue));
        assert!(is_outstanding_work(&TaskStatus::InProgress));
        assert!(is_outstanding_work(&TaskStatus::HumanReview));
        assert!(is_outstanding_work(&TaskStatus::AiReview));
        assert!(is_outstanding_work(&TaskStatus::PrCreated));
        assert!(is_outstanding_work(&TaskStatus::Error));
        assert!(!is_outstanding_work(&TaskStatus::Backlog));
        assert!(!is_outstanding_work(&TaskStatus::Done));
    }
}
