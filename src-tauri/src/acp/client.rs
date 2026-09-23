use super::protocol::*;
use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use tokio::sync::Mutex;
use std::collections::HashMap;
use tokio::sync::{broadcast, oneshot};

type PendingRequests = Arc<Mutex<HashMap<String, oneshot::Sender<AcpResponse>>>>;

pub struct AcpClient {
    pub child: Arc<Mutex<Child>>,
    next_id: Arc<Mutex<u64>>,
    pending_requests: PendingRequests,
    notification_tx: broadcast::Sender<AcpNotification>,
}

impl AcpClient {
    /// Start an agent process in `working_dir`.
    ///
    /// The directory is a required argument rather than an inherited default.
    /// This used to set none at all, so the agent ran in whatever directory
    /// SlashIt itself had been launched from -- for a desktop application,
    /// typically the user's home or `/`, and never anything the caller chose.
    /// An agent's working directory decides what it can read and write, so it
    /// is not something to leave to the process that happened to start the app.
    pub fn start(
        command: &str,
        args: &[&str],
        env: &[(&str, &str)],
        working_dir: &std::path::Path,
    ) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .current_dir(working_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        for (key, value) in env {
            cmd.env(key, value);
        }

        let child = cmd
            .spawn()
            .context("Failed to spawn agent process")?;

        let (notification_tx, _) = broadcast::channel(256);

        let client = Self {
            child: Arc::new(Mutex::new(child)),
            next_id: Arc::new(Mutex::new(1)),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            notification_tx,
        };

        client.start_response_reader();

        Ok(client)
    }

    /// Subscribe to agent notifications (log, status, etc.)
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<AcpNotification> {
        self.notification_tx.subscribe()
    }

    fn start_response_reader(&self) {
        let child = self.child.clone();
        let pending_requests = self.pending_requests.clone();
        let notification_tx = self.notification_tx.clone();

        tokio::spawn(async move {
            let mut child_guard = child.lock().await;
            if let Some(stdout) = child_guard.stdout.as_mut() {
                let reader = BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    if let Ok(notification) = serde_json::from_str::<AcpNotification>(&line) {
                        let _ = notification_tx.send(notification);
                    } else if let Ok(response) = serde_json::from_str::<AcpResponse>(&line) {
                        if let Some(id) = response.id.as_ref() {
                            let mut requests = pending_requests.lock().await;
                            if let Some(tx) = requests.remove(id) {
                                let _ = tx.send(response);
                            }
                        }
                    }
                }
            }
        });
    }

    pub async fn initialize(&self, name: String, version: String) -> Result<InitializeResult> {
        let request = AcpRequest::Initialize {
            params: InitializeParams {
                name,
                version,
                capabilities: ClientCapabilities {
                    experimental: None,
                },
            },
        };

        let response = self.send_request(request).await?;
        let result = response.result.context("Initialize returned error")?;

        serde_json::from_value(result).context("Failed to parse initialize result")
    }

    pub async fn create_session(&self, name: String) -> Result<String> {
        let request = AcpRequest::Create {
            params: CreateParams {
                options: CreateOptions { name },
            },
        };

        let response = self.send_request(request).await?;
        let result = response.result.context("Create returned error")?;

        let create_result: CreateResult =
            serde_json::from_value(result).context("Failed to parse create result")?;
        Ok(create_result.session_id)
    }

    pub async fn send_prompt(&self, session_id: String, prompt: String) -> Result<SendPromptResult> {
        let request = AcpRequest::SendPrompt {
            params: SendPromptParams {
                session_id,
                prompt,
            },
        };

        let response = self.send_request(request).await?;

        if let Some(error) = response.error {
            return Ok(SendPromptResult {
                result: Some(error.message),
                is_error: true,
            });
        }

        if let Some(result) = response.result {
            let parsed: SendPromptResult = serde_json::from_value(result)
                .unwrap_or(SendPromptResult {
                    result: None,
                    is_error: false,
                });
            Ok(parsed)
        } else {
            Ok(SendPromptResult {
                result: None,
                is_error: false,
            })
        }
    }

    pub async fn stop(&self, session_id: String) -> Result<()> {
        let request = AcpRequest::Stop {
            params: StopParams { session_id },
        };

        self.send_request(request).await?;
        Ok(())
    }

    async fn send_request(&self, request: AcpRequest) -> Result<AcpResponse> {
        let id = {
            let mut next_id = self.next_id.lock().await;
            let id = *next_id;
            *next_id += 1;
            id.to_string()
        };

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending_requests.lock().await;
            pending.insert(id.clone(), tx);
        }

        let request_value = serde_json::to_value(request)?;
        let method = request_value.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = request_value.get("params");

        let mut jsonrpc_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
        });

        if let Some(params) = params {
            if let Some(obj) = jsonrpc_request.as_object_mut() {
                obj.insert("params".to_string(), params.clone());
            }
        }

        let mut child = self.child.lock().await;

        if let Some(stdin) = child.stdin.as_mut() {
            writeln!(stdin, "{}", jsonrpc_request)
                .context("Failed to write to agent stdin")?;
            stdin.flush().context("Failed to flush agent stdin")?;
        }

        drop(child);

        tokio::time::timeout(
            tokio::time::Duration::from_secs(300), // 5 min for long prompts
            rx
        )
        .await
        .context("Request timeout")?
        .context("Channel closed")
    }

    pub async fn kill(&self) -> Result<()> {
        let mut child = self.child.lock().await;
        child.kill().context("Failed to kill agent process")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// The spawned process's cwd is the directory the caller named, not
    /// wherever SlashIt itself happened to be launched from.
    ///
    /// Writes to a file rather than reading captured stdout, because
    /// `start_response_reader` (spawned inside `start`) takes ownership of
    /// the child's stdout for the agent's own notification stream; racing it
    /// for the same pipe would be a second bug layered on the one this test
    /// is isolating.
    #[tokio::test]
    async fn start_spawns_the_process_in_the_given_working_dir() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let marker = tmp.path().join("cwd.txt");

        let client = AcpClient::start(
            "sh",
            &["-c", "pwd > cwd.txt"],
            &[],
            tmp.path(),
        )
        .expect("failed to spawn");

        // Wait for the child to exit so the write has landed.
        let status = {
            let mut child = client.child.lock().await;
            child.wait().expect("child did not exit")
        };
        assert!(status.success());

        let recorded = std::fs::read_to_string(&marker)
            .expect("the child never wrote its cwd — it did not run in the given directory");
        let recorded = Path::new(recorded.trim())
            .canonicalize()
            .expect("recorded cwd should resolve");
        let expected = tmp.path().canonicalize().expect("tempdir should resolve");
        assert_eq!(
            recorded, expected,
            "the spawned process's cwd must be exactly the directory passed to start()"
        );
    }
}
