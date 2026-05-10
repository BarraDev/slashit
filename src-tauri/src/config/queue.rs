use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct QueueConfig {
    pub parallel_task_limit: u32,
    pub auto_promote: bool,
    pub fifo_ordering: bool,
    pub use_coderabbit: bool,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            parallel_task_limit: 3,
            auto_promote: true,
            fifo_ordering: true,
            use_coderabbit: true,
        }
    }
}
