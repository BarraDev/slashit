// SPDX-License-Identifier: MPL-2.0
//! The models a coding runtime can be asked to use, for the model picker.
//!
//! An entry is a name the picker may offer, never a proof of access. OpenCode
//! entries come from the runtime's own `opencode models` list. Claude Code's
//! entries are its documented aliases, which no runtime catalog lists. Provider
//! access is checked only when a run starts.
//!
//! A failed refresh never discards the last good list: the caller gets that
//! list with the error, so a Task's saved model stays selectable.

use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;

use crate::domain::task::{is_opencode_model_id, CodingProvider};

/// Bounds one `opencode models` execution.
const OPENCODE_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(20);

/// The most stdout one `opencode models` execution may write. A listing is a few
/// hundred short lines, so a larger stream is a runaway process, not a catalog.
const OPENCODE_DISCOVERY_MAX_OUTPUT: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeModel {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelCatalog {
    pub provider: CodingProvider,
    pub models: Vec<RuntimeModel>,
    /// Why the list is not fresh: a refresh failed, or the runtime is missing.
    /// The models shown are the last good list, when one exists.
    pub error: Option<String>,
}

/// The last successful OpenCode discovery, kept for the process lifetime.
static OPENCODE_CACHE: Mutex<OpenCodeCache> = Mutex::new(OpenCodeCache { started: 0, kept: None });

#[derive(Debug, Default)]
struct OpenCodeCache {
    /// Discoveries started so far. Each one takes the next number when it starts.
    started: u64,
    /// The list kept, with the number of the discovery that produced it.
    kept: Option<(u64, Vec<RuntimeModel>)>,
}

fn locked(cache: &Mutex<OpenCodeCache>) -> std::sync::MutexGuard<'_, OpenCodeCache> {
    cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Claude Code's model aliases. Always listed, whether or not the CLI is
/// installed. No reliable non-interactive account listing exists, so they are
/// never claimed as verified. Pinned model IDs are not listed, because a fixed ID
/// goes stale; a person who needs one types it, and a saved one stays selected.
pub fn claude_models() -> Vec<RuntimeModel> {
    let entry = |id: &str, name: &str| RuntimeModel {
        id: id.to_string(),
        name: name.to_string(),
    };
    vec![
        entry("default", "Default (auto)"),
        entry("sonnet", "Claude Sonnet (latest alias)"),
        entry("opus", "Claude Opus (latest alias)"),
        entry("haiku", "Claude Haiku (latest alias)"),
    ]
}

/// Parses `opencode models` output: one `provider/model` per line. Blank lines
/// and anything that is not a provider/model id are skipped, and a repeated id
/// is kept once, in its first position.
pub fn parse_opencode_models(stdout: &str) -> Vec<RuntimeModel> {
    let mut seen = std::collections::HashSet::new();
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| is_opencode_model_id(line))
        .filter(|line| seen.insert(line.to_string()))
        .map(|id| RuntimeModel {
            id: id.to_string(),
            name: id.to_string(),
        })
        .collect()
}

/// Reads at most `limit` bytes from `reader`. A stream longer than that is an
/// error, so a runaway process cannot grow the buffer without bound.
async fn read_capped(reader: impl tokio::io::AsyncRead + Unpin, limit: u64) -> Result<Vec<u8>, String> {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    // One byte past the limit tells an exact-size stream from a longer one.
    reader
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| format!("OpenCode output could not be read: {e}"))?;
    if bytes.len() as u64 > limit {
        return Err(format!("OpenCode listed more than {limit} bytes of models"));
    }
    Ok(bytes)
}

/// Kills the process group `pid` leads, so a private server it started does not
/// outlive the listing. The leader itself is also killed on drop.
fn kill_group(group: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = group {
        // Safety: `pid` is the leader of the group made by `process_group(0)`.
        unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) };
    }
    #[cfg(not(unix))]
    let _ = group;
}

/// Runs `opencode models` under the same isolated configuration a Worker gets,
/// so the picker lists only models a Worker can select. `--standalone` keeps the
/// listing off the shared background service, which does not read this
/// environment. Stdout is capped and the process group is killed when the
/// listing fails, overflows or passes its deadline.
async fn run_opencode_models() -> Result<Vec<RuntimeModel>, String> {
    let config_dir = tempfile::Builder::new()
        .prefix("slashit-opencode-discovery-")
        .tempdir()
        .map_err(|e| format!("The OpenCode discovery directory could not be created: {e}"))?;
    let config_path = config_dir
        .path()
        .to_str()
        .ok_or_else(|| "The OpenCode discovery directory path is not UTF-8".to_string())?
        .to_string();
    run_listing(
        crate::agents::opencode_run::OPENCODE_PROGRAM,
        &["models", "--standalone"],
        &config_path,
        OPENCODE_DISCOVERY_TIMEOUT,
        OPENCODE_DISCOVERY_MAX_OUTPUT,
    )
    .await
}

/// Runs `program args` with the Worker's environment under `config_path`, and
/// parses its stdout. Separated from [`run_opencode_models`] so the timeout,
/// output bound and process-group cleanup can be tested with a fake program.
async fn run_listing(
    program: &str,
    args: &[&str],
    config_path: &str,
    timeout: Duration,
    max_output: u64,
) -> Result<Vec<RuntimeModel>, String> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .env_clear()
        .envs(crate::agents::opencode_run::opencode_spawn_env(
            std::env::vars_os(),
            config_path,
        ))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|e| format!("OpenCode could not be started: {e}"))?;
    let group = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "OpenCode output is not piped".to_string())?;
    let listing = async {
        let bytes = read_capped(stdout, max_output).await?;
        let status = child
            .wait()
            .await
            .map_err(|e| format!("OpenCode could not be run: {e}"))?;
        Ok::<_, String>((status, bytes))
    };
    let (status, bytes) = match tokio::time::timeout(timeout, listing).await {
        Ok(Ok(listed)) => {
            // A background process that kept its stdout closed would otherwise outlive the listing.
            kill_group(group);
            listed
        }
        Ok(Err(error)) => {
            kill_group(group);
            return Err(error);
        }
        Err(_) => {
            kill_group(group);
            return Err("OpenCode did not list its models in time".to_string());
        }
    };
    if !status.success() {
        return Err(format!("OpenCode exited with {status} while listing its models"));
    }
    let models = parse_opencode_models(&String::from_utf8_lossy(&bytes));
    if models.is_empty() {
        return Err("OpenCode listed no models".to_string());
    }
    Ok(models)
}

/// The catalog for `provider`. `refresh` runs discovery again; otherwise a
/// cached OpenCode list is returned without running the CLI.
pub async fn catalog(provider: CodingProvider, refresh: bool) -> ModelCatalog {
    match provider {
        CodingProvider::Claude => ModelCatalog {
            provider,
            models: claude_models(),
            error: None,
        },
        CodingProvider::OpenCode => {
            opencode_catalog(&OPENCODE_CACHE, refresh, run_opencode_models()).await
        }
    }
}

/// The OpenCode catalog. `discover` is started only when there is no cached
/// list or a refresh was asked for, so a cached read runs no process.
async fn opencode_catalog(
    cache: &Mutex<OpenCodeCache>,
    refresh: bool,
    discover: impl std::future::Future<Output = Result<Vec<RuntimeModel>, String>>,
) -> ModelCatalog {
    if !refresh {
        if let Some((_, cached)) = locked(cache).kept.clone() {
            return ModelCatalog {
                provider: CodingProvider::OpenCode,
                models: cached,
                error: None,
            };
        }
    }
    // The number is taken when the discovery starts. Two refreshes can finish in
    // either order, so a result replaces the kept list only if its discovery started later.
    let number = {
        let mut cache = locked(cache);
        cache.started += 1;
        cache.started
    };
    match discover.await {
        Ok(models) => {
            let mut cache = locked(cache);
            if cache.kept.as_ref().is_none_or(|(kept, _)| *kept < number) {
                cache.kept = Some((number, models.clone()));
            }
            ModelCatalog {
                provider: CodingProvider::OpenCode,
                models,
                error: None,
            }
        }
        Err(error) => {
            // Keep the last good list: a failed refresh must not empty the
            // picker or drop a saved model.
            let models = locked(cache).kept.as_ref().map(|(_, models)| models.clone()).unwrap_or_default();
            ModelCatalog {
                provider: CodingProvider::OpenCode,
                models,
                error: Some(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opencode_output_keeps_provider_model_lines_once_in_order() {
        let stdout = "ollama/llama3.2:3b\n\n  opencode-go/beam-free  \nnot a model\n/leading\ntrailing/\nollama/llama3.2:3b\nbad id/with space\n";
        let ids: Vec<String> = parse_opencode_models(stdout).into_iter().map(|m| m.id).collect();
        assert_eq!(ids, vec!["ollama/llama3.2:3b", "opencode-go/beam-free"]);
    }

    #[test]
    fn claude_entries_are_the_documented_aliases_in_order() {
        let ids: Vec<String> = claude_models().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, vec!["default", "sonnet", "opus", "haiku"]);
    }

    #[tokio::test]
    async fn a_listing_within_the_bound_is_read_whole() {
        let bytes = read_capped(&b"ollama/llama3.2:3b\n"[..], 64).await.expect("within the bound");
        assert_eq!(bytes, b"ollama/llama3.2:3b\n");
    }

    /// A stream that never ends would otherwise grow the buffer until it fails.
    /// The reader here is endless, so the read must stop at the bound.
    #[tokio::test]
    async fn an_endless_listing_stops_at_the_bound() {
        let error = read_capped(tokio::io::repeat(b'a'), 1024)
            .await
            .expect_err("an endless stream is refused");
        assert!(error.contains("more than 1024 bytes"), "{error}");
    }

    #[tokio::test]
    async fn a_listing_of_exactly_the_bound_is_accepted() {
        let exact = [b'a'; 16];
        let bytes = read_capped(&exact[..], 16).await.expect("exactly the bound is allowed");
        assert_eq!(bytes.len(), 16);
    }

    /// Runs a shell script as the discovery program. `/bin/sh` is used directly
    /// so no executable file is written, which avoids a busy-binary race.
    #[cfg(unix)]
    fn shell_listing(script: &str, timeout: Duration, max_output: u64) -> impl std::future::Future<Output = Result<Vec<RuntimeModel>, String>> {
        let script = script.to_string();
        async move { run_listing("/bin/sh", &["-c", &script], "/unused", timeout, max_output).await }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_listing_that_overflows_its_bound_is_refused_and_stopped() {
        let started = std::time::Instant::now();
        let error = shell_listing(
            "while :; do echo ollama/llama3.2:3b; done",
            Duration::from_secs(20),
            4096,
        )
        .await
        .expect_err("an endless listing is refused");
        assert!(error.contains("more than 4096 bytes"), "{error}");
        // Overflow ends the run at once, not at the 20-second deadline.
        assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_listing_with_a_nonzero_exit_is_an_error() {
        let error = shell_listing("exit 3", Duration::from_secs(5), 1024)
            .await
            .expect_err("a failed listing is an error");
        assert!(error.contains("exited"), "{error}");
    }

    /// The Linux process table reports a zombie as `Z`, so a reaped-or-zombie
    /// child counts as stopped.
    #[cfg(target_os = "linux")]
    fn process_is_running(pid: u32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => !stat
                .rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
            Err(_) => false,
        }
    }

    /// A deadline stops the listing and everything it started, not only its
    /// leader: the background `sleep` below is in the same process group.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_listing_past_its_deadline_stops_its_whole_process_group() {
        let dir = tempfile::tempdir().expect("scratch directory");
        let pid_file = dir.path().join("child.pid");
        let script = format!(
            "/bin/sleep 30 & echo $! > '{}'; /bin/sleep 30",
            pid_file.display()
        );
        let error = shell_listing(&script, Duration::from_secs(2), 1024)
            .await
            .expect_err("a listing past its deadline fails");
        assert!(error.contains("in time"), "{error}");

        let child: u32 = std::fs::read_to_string(&pid_file)
            .expect("the script recorded its background child")
            .trim()
            .parse()
            .expect("a pid");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while process_is_running(child) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!process_is_running(child), "background child {child} survived the deadline");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_refresh_keeps_the_last_good_list_and_reports_why() {
        let cache = Mutex::new(OpenCodeCache::default());
        let good = opencode_catalog(
            &cache,
            true,
            shell_listing("echo ollama/llama3.2:3b", Duration::from_secs(5), 1024),
        )
        .await;
        assert_eq!(good.error, None);

        let failed = opencode_catalog(
            &cache,
            true,
            shell_listing("exit 1", Duration::from_secs(5), 1024),
        )
        .await;
        assert!(failed.error.is_some(), "the failed refresh is reported");
        assert_eq!(failed.models, good.models, "the last good list is kept");
        assert_eq!(failed.models[0].id, "ollama/llama3.2:3b");
    }

    /// A successful listing whose background child closed its stdout still
    /// stops that child: the pipe reaches end-of-file, so only the group kill ends it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_successful_listing_stops_a_background_child_that_closed_its_output() {
        let dir = tempfile::tempdir().expect("scratch directory");
        let pid_file = dir.path().join("child.pid");
        let script = format!(
            "/bin/sleep 30 >/dev/null 2>&1 & echo $! > '{}'; echo ollama/llama3.2:3b",
            pid_file.display()
        );
        let models = shell_listing(&script, Duration::from_secs(5), 1024)
            .await
            .expect("the listing succeeds");
        assert_eq!(models[0].id, "ollama/llama3.2:3b");

        let child: u32 = std::fs::read_to_string(&pid_file)
            .expect("the script recorded its background child")
            .trim()
            .parse()
            .expect("a pid");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while process_is_running(child) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!process_is_running(child), "background child {child} outlived the listing");
    }

    #[tokio::test]
    async fn a_cached_list_is_returned_without_running_discovery() {
        let cache = Mutex::new(OpenCodeCache {
            started: 1,
            kept: Some((1, parse_opencode_models("ollama/llama3.2:3b\n"))),
        });
        let cached = opencode_catalog(
            &cache,
            false,
            std::future::ready(Err("discovery must not run for a cached list".to_string())),
        )
        .await;
        assert_eq!(cached.error, None);
        assert_eq!(cached.models[0].id, "ollama/llama3.2:3b");
    }

    /// Two refreshes overlap and the older one finishes last. Its list must not
    /// replace the newer list the cache already holds.
    #[tokio::test]
    async fn an_older_refresh_that_finishes_last_does_not_replace_a_newer_list() {
        let cache = Mutex::new(OpenCodeCache::default());
        let (older_started, started) = tokio::sync::oneshot::channel::<()>();
        let (release_older, released) = tokio::sync::oneshot::channel::<()>();
        let (older, newer) = tokio::join!(
            opencode_catalog(&cache, true, async {
                older_started.send(()).expect("the newer refresh waits for this");
                released.await.expect("the older refresh is released");
                Ok(parse_opencode_models("ollama/older:1b\n"))
            }),
            async {
                // The older refresh has started, so it holds the earlier number.
                started.await.expect("the older refresh started");
                let newer = opencode_catalog(
                    &cache,
                    true,
                    std::future::ready(Ok(parse_opencode_models("ollama/newer:1b\n"))),
                )
                .await;
                release_older.send(()).expect("the older refresh is waiting");
                newer
            },
        );
        assert_eq!(older.models[0].id, "ollama/older:1b");
        assert_eq!(newer.models[0].id, "ollama/newer:1b");

        let kept = opencode_catalog(
            &cache,
            false,
            std::future::ready(Err("discovery must not run for a cached list".to_string())),
        )
        .await;
        assert_eq!(kept.models[0].id, "ollama/newer:1b", "the newer list is what the cache keeps");
    }
}
