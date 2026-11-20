// SPDX-License-Identifier: MPL-2.0
//! The Project Rail's order and hidden Projects, as the backend stores them.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::Project;

/// Mirrors `config::ProjectRailConfig`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRailConfig {
    #[serde(default)]
    pub order: Vec<Uuid>,
    #[serde(default)]
    pub hidden: Vec<Uuid>,
}

/// Splits Projects into the rail's visible list and the hidden list. Listed
/// Projects keep the configured order; unlisted ones follow by creation time.
pub fn arrange(projects: &[Project], config: &ProjectRailConfig) -> (Vec<Project>, Vec<Project>) {
    let mut ordered = projects.to_vec();
    ordered.sort_by_key(|project| {
        let position = config.order.iter().position(|id| *id == project.id).unwrap_or(usize::MAX);
        (position, project.created_at)
    });
    ordered.into_iter().partition(|project| !config.hidden.contains(&project.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::AgentType;
    use chrono::{TimeZone, Utc};

    fn project(id: u128, day: u32) -> Project {
        Project {
            id: Uuid::from_u128(id),
            name: format!("p{id}"),
            repository_id: None,
            scope: Default::default(),
            agent_type: AgentType::ClaudeCode,
            created_at: Utc.with_ymd_and_hms(2026, 1, day, 0, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 1, day, 0, 0, 0).unwrap(),
        }
    }

    #[test]
    fn listed_order_comes_first_then_creation_time() {
        let projects = vec![project(1, 1), project(2, 2), project(3, 3)];
        let config = ProjectRailConfig { order: vec![Uuid::from_u128(3), Uuid::from_u128(1)], hidden: vec![] };
        let (visible, hidden) = arrange(&projects, &config);
        let ids: Vec<u128> = visible.iter().map(|p| p.id.as_u128()).collect();
        assert_eq!(ids, vec![3, 1, 2]);
        assert!(hidden.is_empty());
    }

    #[test]
    fn hidden_projects_leave_the_visible_list_and_keep_their_identity() {
        let projects = vec![project(1, 1), project(2, 2)];
        let config = ProjectRailConfig { order: vec![], hidden: vec![Uuid::from_u128(1)] };
        let (visible, hidden) = arrange(&projects, &config);
        assert_eq!(visible.iter().map(|p| p.id).collect::<Vec<_>>(), vec![Uuid::from_u128(2)]);
        assert_eq!(hidden.iter().map(|p| p.id).collect::<Vec<_>>(), vec![Uuid::from_u128(1)]);
        assert_eq!(hidden[0].name, "p1");
    }

    #[test]
    fn a_config_that_hides_everything_yields_an_empty_visible_list() {
        let projects = vec![project(1, 1)];
        let config = ProjectRailConfig { order: vec![], hidden: vec![Uuid::from_u128(1)] };
        let (visible, _) = arrange(&projects, &config);
        assert!(visible.is_empty());
    }
}
