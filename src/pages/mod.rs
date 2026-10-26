// SPDX-License-Identifier: MPL-2.0
#![allow(unused_imports)]

pub mod conversations;
pub mod dashboard;
pub mod agent;
pub mod context;
pub mod settings;
pub mod roadmap;
pub mod ideation;
pub mod insights;
pub mod changelog;
pub mod mcp;
pub mod worktrees;
pub mod workspaces;
pub mod github_issues;
pub mod github_prs;

pub use conversations::*;
pub use dashboard::*;
pub use agent::*;
pub use context::*;
pub use settings::*;
pub use roadmap::*;
pub use ideation::*;
pub use insights::*;
pub use changelog::*;
pub use mcp::*;
pub use worktrees::*;
pub use workspaces::*;
pub use github_issues::*;
pub use github_prs::*;

/// Pages a person cannot yet use. Roadmap has no control to create a feature
/// (the backend command exists, the UI does not call it), so it has nothing
/// useful to show. These pages stay compiled and routable, but the sidebar and
/// the keyboard shortcuts do not offer them.
pub const UNFINISHED_PAGES: [&str; 5] = ["roadmap", "worktrees", "ideation", "changelog", "mcp"];

/// Set to show the unfinished pages again. Keep it false until they work.
pub const SHOW_UNFINISHED_PAGES: bool = false;

/// Whether the navigation offers `page` to the person.
pub fn page_offered(page: &str) -> bool {
    SHOW_UNFINISHED_PAGES || !UNFINISHED_PAGES.contains(&page)
}

/// Every page name the router renders. Keep it in step with the match in
/// `app.rs`. A name outside this list was removed, or was never a page.
pub const ROUTED_PAGES: [&str; 14] = [
    "conversations",
    "dashboard",
    "agent",
    "roadmap",
    "ideation",
    "context",
    "settings",
    "insights",
    "changelog",
    "mcp",
    "worktrees",
    "workspaces",
    "github_issues",
    "github_prs",
];

/// Whether a page name stored from an earlier run may be restored: it must
/// still be a page, and the navigation must offer it.
pub fn page_restorable(page: &str) -> bool {
    ROUTED_PAGES.contains(&page) && page_offered(page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_removed_page_name_is_not_restored() {
        assert!(!page_restorable("spec"), "spec was removed and has no route");
        assert!(!page_restorable(""));
    }

    #[test]
    fn every_routed_page_is_restorable_exactly_when_it_is_offered() {
        for page in ROUTED_PAGES {
            assert_eq!(page_restorable(page), page_offered(page), "{page}");
        }
        assert!(page_restorable("dashboard"));
    }
}

/// Roadmap controls for features that are not built yet: the Edit button has
/// no handler, and Convert to task calls a command the backend does not
/// register. Keep false until both work.
pub const SHOW_UNFINISHED_FEATURES: bool = false;
