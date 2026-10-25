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

/// Roadmap controls for features that are not built yet: the Edit button has
/// no handler, and Convert to task calls a command the backend does not
/// register. Keep false until both work.
pub const SHOW_UNFINISHED_FEATURES: bool = false;
