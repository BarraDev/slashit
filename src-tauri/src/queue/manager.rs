use crate::config::QueueConfig;
use crate::domain::{Task, TaskStatus, TaskPhase};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

type Tasks = Arc<RwLock<HashMap<Uuid, Task>>>;

#[derive(Clone)]
pub struct QueueManager {
    tasks: Tasks,
    config: QueueConfig,
}

impl QueueManager {
    pub fn new(tasks: Tasks, config: QueueConfig) -> Self {
        Self { tasks, config }
    }

    pub fn with_config(tasks: Tasks) -> Self {
        Self {
            tasks,
            config: QueueConfig::default(),
        }
    }

    pub fn config(&self) -> &QueueConfig {
        &self.config
    }

    pub async fn set_config(&mut self, config: QueueConfig) {
        self.config = config;
    }

    pub async fn can_start_task(&self) -> bool {
        let tasks = self.tasks.read().await;
        let in_progress_count = tasks
            .values()
            .filter(|t| t.status == TaskStatus::InProgress)
            .count();
        in_progress_count < self.config.parallel_task_limit as usize
    }

    /// Which queued task would be promoted next, or `None` if there is
    /// nothing to promote right now.
    ///
    /// Pure: takes no lock and writes nothing. Selection has to run under the
    /// same write-lock hold the caller then commits the promotion with --
    /// see `lifecycle::commit_selected` -- or two callers racing this could
    /// both select the same task before either one's write lands. This is
    /// why the signature takes the map directly rather than reading
    /// `self.tasks` itself.
    pub(crate) fn select_promotable(&self, tasks: &HashMap<Uuid, Task>) -> Option<Uuid> {
        let in_progress_count = tasks
            .values()
            .filter(|t| t.status == TaskStatus::InProgress)
            .count();
        if in_progress_count >= self.config.parallel_task_limit as usize {
            return None;
        }

        let queued_tasks: Vec<_> = tasks
            .values()
            .filter(|t| t.status == TaskStatus::Queue)
            // A task whose cleanup was interrupted cannot be executed -- see
            // `TaskExecutor::is_pending` -- so promoting it would only park it
            // in a column that claims it is running while nothing is. Worse,
            // the promotion below clears `error_message`, which for a
            // quarantined task is the one place the interrupted cleanup is
            // explained to the user.
            .filter(|t| !t.cleanup_in_flight)
            .collect();

        if self.config.fifo_ordering {
            queued_tasks.into_iter().min_by_key(|t| t.created_at).map(|t| t.id)
        } else {
            queued_tasks.first().map(|t| t.id)
        }
    }

    /// The fields a promotion resets, applied by every caller of
    /// [`Self::select_promotable`] so the desktop command, the daemon's
    /// automatic poll, and any future front door agree on what "promoted"
    /// means.
    pub(crate) fn apply_promotion(task: &mut Task) {
        task.status = TaskStatus::InProgress;
        task.phase = TaskPhase::Idle;
        task.phase_progress = 0;
        task.overall_progress = 0;
        task.error_message = None;
    }

    pub async fn get_queued_tasks(&self) -> Vec<Task> {
        let tasks = self.tasks.read().await;
        let mut queued_tasks: Vec<_> = tasks
            .values()
            .filter(|t| t.status == TaskStatus::Queue)
            .cloned()
            .collect();

        if self.config.fifo_ordering {
            queued_tasks.sort_by_key(|t| t.created_at);
        }

        queued_tasks
    }

    pub async fn get_in_progress_count(&self) -> usize {
        let tasks = self.tasks.read().await;
        tasks
            .values()
            .filter(|t| t.status == TaskStatus::InProgress)
            .count()
    }

    pub async fn get_capacity_available(&self) -> usize {
        let in_progress = self.get_in_progress_count().await;
        self.config.parallel_task_limit.saturating_sub(in_progress as u32) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TaskCategory;

    #[tokio::test]
    async fn test_can_start_task() {
        let tasks = Arc::new(RwLock::new(HashMap::new()));
        let manager = QueueManager::with_config(tasks.clone());

        assert!(manager.can_start_task().await);
    }

    #[tokio::test]
    async fn test_capacity_limit() {
        let tasks = Arc::new(RwLock::new(HashMap::new()));
        let config = QueueConfig {
            parallel_task_limit: 2,
            ..Default::default()
        };
        let manager = QueueManager::new(tasks.clone(), config);

        let mut task_store = tasks.write().await;
        for i in 0..2 {
            let task = Task {
                id: Uuid::new_v4(),
                project_id: Uuid::new_v4(),
                title: format!("Task {}", i),
                description: None,
                status: TaskStatus::InProgress,
                model: "test".to_string(),
                planning_mode: false,
                dependencies: Vec::new(),
                worktree_id: None,
                jj_change_id: None,
                category: TaskCategory::Feature,
                priority: Default::default(),
                complexity: Default::default(),
                impact: Default::default(),
                security_severity: Default::default(),
                phase: Default::default(),
                phase_progress: 0,
                overall_progress: 0,
                subtasks: Vec::new(),
                sequence_number: 0,
                github_issue_url: None,
                gitlab_issue_url: None,
                linear_ticket_id: None,
                jira_issue_key: None,
                pr_url: None,
                external_refs: Vec::new(),
                qa_signoff: None,
                human_review: None,
                stuck_since: None,
                error_message: None,
                worktree_path: None,
                branch_name: None,
                cleanup_in_flight: false,
                position: 0,
                pr_review_plan: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            };
            task_store.insert(task.id, task);
        }
        drop(task_store);

        assert!(!manager.can_start_task().await);
        assert_eq!(manager.get_capacity_available().await, 0);
    }

    // --- Helper ---

    use crate::test_helpers::create_test_task_with_status;
    use chrono::{Duration, Utc};

    fn make_tasks_map(task_list: Vec<Task>) -> Tasks {
        let mut map = HashMap::new();
        for t in task_list {
            map.insert(t.id, t);
        }
        Arc::new(RwLock::new(map))
    }

    fn make_manager(tasks: Tasks, limit: u32) -> QueueManager {
        QueueManager::new(
            tasks,
            QueueConfig {
                parallel_task_limit: limit,
                fifo_ordering: true,
                ..Default::default()
            },
        )
    }

    // === select_promotable ===
    //
    // Pure selection now, with no map of its own to mutate: these tests apply
    // `QueueManager::apply_promotion` themselves and write the result back,
    // exactly what `lifecycle::commit_selected` does under its own write
    // lock in production. Durability itself -- the reason the mutating
    // `promote_next_task` this replaced no longer exists -- is proven at the
    // `commands::queue` and `queue::executor` front doors instead.

    /// Mirrors the production sequence `commit_selected` runs, but against a
    /// plain map with no persistence, since these tests are about selection
    /// and field-reset semantics, not durability.
    async fn promote(manager: &QueueManager, tasks: &Tasks) -> Option<Uuid> {
        let mut store = tasks.write().await;
        let task_id = manager.select_promotable(&store)?;
        let task = store.get_mut(&task_id).expect("selected id must be in the map");
        QueueManager::apply_promotion(task);
        Some(task_id)
    }

    #[tokio::test]
    async fn test_promote_next_task_empty_queue() {
        let tasks = make_tasks_map(vec![]);
        let manager = make_manager(tasks.clone(), 3);
        assert_eq!(promote(&manager, &tasks).await, None);
    }

    #[tokio::test]
    async fn test_promote_next_task_one_queued() {
        let task = create_test_task_with_status("Queued", TaskStatus::Queue);
        let task_id = task.id;
        let tasks = make_tasks_map(vec![task]);
        let manager = make_manager(tasks.clone(), 3);

        let result = promote(&manager, &tasks).await;
        assert_eq!(result, Some(task_id));

        let store = tasks.read().await;
        let promoted = store.get(&task_id).unwrap();
        assert_eq!(promoted.status, TaskStatus::InProgress);
    }

    #[tokio::test]
    async fn test_promote_next_task_at_capacity() {
        let in_progress = create_test_task_with_status("Running", TaskStatus::InProgress);
        let queued = create_test_task_with_status("Waiting", TaskStatus::Queue);
        let tasks = make_tasks_map(vec![in_progress, queued]);
        let manager = make_manager(tasks.clone(), 1);

        assert_eq!(promote(&manager, &tasks).await, None);
    }

    #[tokio::test]
    async fn test_promote_next_task_fifo_ordering() {
        let mut older = create_test_task_with_status("Older", TaskStatus::Queue);
        older.created_at = Utc::now() - Duration::hours(2);
        let older_id = older.id;

        let mut newer = create_test_task_with_status("Newer", TaskStatus::Queue);
        newer.created_at = Utc::now() - Duration::hours(1);

        let tasks = make_tasks_map(vec![newer, older]);
        let manager = make_manager(tasks.clone(), 3);

        let result = promote(&manager, &tasks).await;
        assert_eq!(result, Some(older_id));
    }

    #[tokio::test]
    async fn test_promote_next_task_resets_fields() {
        let mut task = create_test_task_with_status("Queued", TaskStatus::Queue);
        task.phase = TaskPhase::Coding;
        task.phase_progress = 50;
        task.overall_progress = 75;
        task.error_message = Some("old error".to_string());
        let task_id = task.id;
        let tasks = make_tasks_map(vec![task]);
        let manager = make_manager(tasks.clone(), 3);

        promote(&manager, &tasks).await;

        let store = tasks.read().await;
        let promoted = store.get(&task_id).unwrap();
        assert_eq!(promoted.phase, TaskPhase::Idle);
        assert_eq!(promoted.phase_progress, 0);
        assert_eq!(promoted.overall_progress, 0);
        assert!(promoted.error_message.is_none());
    }

    #[tokio::test]
    async fn test_promote_next_task_excludes_a_quarantined_task() {
        // A task whose cleanup was interrupted must not be handed to the
        // executor even though it is otherwise queued and capacity is free --
        // see the comment on `select_promotable` for why.
        let mut quarantined = create_test_task_with_status("Quarantined", TaskStatus::Queue);
        quarantined.cleanup_in_flight = true;
        let tasks = make_tasks_map(vec![quarantined]);
        let manager = make_manager(tasks.clone(), 3);

        assert_eq!(promote(&manager, &tasks).await, None);
    }

    // === get_queued_tasks ===

    #[tokio::test]
    async fn test_get_queued_tasks_empty() {
        let tasks = make_tasks_map(vec![]);
        let manager = make_manager(tasks, 3);
        assert!(manager.get_queued_tasks().await.is_empty());
    }

    #[tokio::test]
    async fn test_get_queued_tasks_multiple() {
        let t1 = create_test_task_with_status("Q1", TaskStatus::Queue);
        let t2 = create_test_task_with_status("Q2", TaskStatus::Queue);
        let tasks = make_tasks_map(vec![t1, t2]);
        let manager = make_manager(tasks, 3);

        let queued = manager.get_queued_tasks().await;
        assert_eq!(queued.len(), 2);
    }

    #[tokio::test]
    async fn test_get_queued_tasks_filters_by_status() {
        let q = create_test_task_with_status("Queued", TaskStatus::Queue);
        let b = create_test_task_with_status("Backlog", TaskStatus::Backlog);
        let ip = create_test_task_with_status("InProgress", TaskStatus::InProgress);
        let tasks = make_tasks_map(vec![q, b, ip]);
        let manager = make_manager(tasks, 3);

        let queued = manager.get_queued_tasks().await;
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].status, TaskStatus::Queue);
    }

    #[tokio::test]
    async fn test_get_queued_tasks_fifo_ordering() {
        let mut older = create_test_task_with_status("Older", TaskStatus::Queue);
        older.created_at = Utc::now() - Duration::hours(3);

        let mut middle = create_test_task_with_status("Middle", TaskStatus::Queue);
        middle.created_at = Utc::now() - Duration::hours(2);

        let mut newer = create_test_task_with_status("Newer", TaskStatus::Queue);
        newer.created_at = Utc::now() - Duration::hours(1);

        let tasks = make_tasks_map(vec![newer, older, middle]);
        let manager = make_manager(tasks, 3);

        let queued = manager.get_queued_tasks().await;
        assert_eq!(queued[0].title, "Older");
        assert_eq!(queued[1].title, "Middle");
        assert_eq!(queued[2].title, "Newer");
    }

    // === get_in_progress_count ===

    #[tokio::test]
    async fn test_get_in_progress_count_none() {
        let tasks = make_tasks_map(vec![]);
        let manager = make_manager(tasks, 3);
        assert_eq!(manager.get_in_progress_count().await, 0);
    }

    #[tokio::test]
    async fn test_get_in_progress_count_mixed_statuses() {
        let ip1 = create_test_task_with_status("IP1", TaskStatus::InProgress);
        let ip2 = create_test_task_with_status("IP2", TaskStatus::InProgress);
        let q = create_test_task_with_status("Queued", TaskStatus::Queue);
        let b = create_test_task_with_status("Backlog", TaskStatus::Backlog);
        let tasks = make_tasks_map(vec![ip1, ip2, q, b]);
        let manager = make_manager(tasks, 3);

        assert_eq!(manager.get_in_progress_count().await, 2);
    }

    // === get_capacity_available ===

    #[tokio::test]
    async fn test_get_capacity_available_no_in_progress() {
        let tasks = make_tasks_map(vec![]);
        let manager = make_manager(tasks, 3);
        assert_eq!(manager.get_capacity_available().await, 3);
    }

    #[tokio::test]
    async fn test_get_capacity_available_partial() {
        let ip1 = create_test_task_with_status("IP1", TaskStatus::InProgress);
        let ip2 = create_test_task_with_status("IP2", TaskStatus::InProgress);
        let tasks = make_tasks_map(vec![ip1, ip2]);
        let manager = make_manager(tasks, 3);

        assert_eq!(manager.get_capacity_available().await, 1);
    }

    #[tokio::test]
    async fn test_get_capacity_available_at_capacity() {
        let ip1 = create_test_task_with_status("IP1", TaskStatus::InProgress);
        let ip2 = create_test_task_with_status("IP2", TaskStatus::InProgress);
        let tasks = make_tasks_map(vec![ip1, ip2]);
        let manager = make_manager(tasks, 2);

        assert_eq!(manager.get_capacity_available().await, 0);
    }

    #[tokio::test]
    async fn test_get_capacity_available_over_capacity_saturates() {
        // Simulate over-capacity (e.g., limit lowered after tasks started)
        let ip1 = create_test_task_with_status("IP1", TaskStatus::InProgress);
        let ip2 = create_test_task_with_status("IP2", TaskStatus::InProgress);
        let ip3 = create_test_task_with_status("IP3", TaskStatus::InProgress);
        let tasks = make_tasks_map(vec![ip1, ip2, ip3]);
        let manager = make_manager(tasks, 2);

        assert_eq!(manager.get_capacity_available().await, 0);
    }

    // === set_config ===

    #[tokio::test]
    async fn test_set_config_changes_capacity() {
        let ip = create_test_task_with_status("IP", TaskStatus::InProgress);
        let tasks = make_tasks_map(vec![ip]);
        let mut manager = make_manager(tasks, 1);

        assert!(!manager.can_start_task().await);

        manager
            .set_config(QueueConfig {
                parallel_task_limit: 2,
                ..Default::default()
            })
            .await;

        assert!(manager.can_start_task().await);
    }

    // === Edge cases ===

    #[tokio::test]
    async fn test_promote_with_only_backlog_tasks() {
        let b1 = create_test_task_with_status("B1", TaskStatus::Backlog);
        let b2 = create_test_task_with_status("B2", TaskStatus::Backlog);
        let tasks = make_tasks_map(vec![b1, b2]);
        let manager = make_manager(tasks.clone(), 3);

        assert_eq!(promote(&manager, &tasks).await, None);
    }

    #[tokio::test]
    async fn test_capacity_limit_zero() {
        let q = create_test_task_with_status("Queued", TaskStatus::Queue);
        let tasks = make_tasks_map(vec![q]);
        let manager = make_manager(tasks.clone(), 0);

        assert!(!manager.can_start_task().await);
        assert_eq!(promote(&manager, &tasks).await, None);
        assert_eq!(manager.get_capacity_available().await, 0);
    }

    #[tokio::test]
    async fn test_capacity_limit_one_sequential() {
        let mut q1 = create_test_task_with_status("Q1", TaskStatus::Queue);
        q1.created_at = Utc::now() - Duration::hours(2);
        let q1_id = q1.id;

        let mut q2 = create_test_task_with_status("Q2", TaskStatus::Queue);
        q2.created_at = Utc::now() - Duration::hours(1);
        let q2_id = q2.id;

        let tasks = make_tasks_map(vec![q1, q2]);
        let manager = make_manager(tasks.clone(), 1);

        // First promote succeeds
        let first = promote(&manager, &tasks).await;
        assert_eq!(first, Some(q1_id));

        // Second promote blocked (at capacity)
        let second = promote(&manager, &tasks).await;
        assert_eq!(second, None);

        // Simulate completion: change first task away from InProgress
        {
            let mut store = tasks.write().await;
            store.get_mut(&q1_id).unwrap().status = TaskStatus::Done;
        }

        // Now second can be promoted
        let third = promote(&manager, &tasks).await;
        assert_eq!(third, Some(q2_id));
    }

    #[tokio::test]
    async fn test_multiple_promotes_until_capacity_hit() {
        let mut q1 = create_test_task_with_status("Q1", TaskStatus::Queue);
        q1.created_at = Utc::now() - Duration::hours(3);

        let mut q2 = create_test_task_with_status("Q2", TaskStatus::Queue);
        q2.created_at = Utc::now() - Duration::hours(2);

        let mut q3 = create_test_task_with_status("Q3", TaskStatus::Queue);
        q3.created_at = Utc::now() - Duration::hours(1);

        let tasks = make_tasks_map(vec![q1, q2, q3]);
        let manager = make_manager(tasks.clone(), 2);

        // First two promotions succeed
        assert!(promote(&manager, &tasks).await.is_some());
        assert!(promote(&manager, &tasks).await.is_some());

        // Third blocked by capacity
        assert_eq!(promote(&manager, &tasks).await, None);
        assert_eq!(manager.get_capacity_available().await, 0);
    }
}
