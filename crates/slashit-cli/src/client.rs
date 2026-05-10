use anyhow::{Context, Result};
use slashit_ipc::{IpcRequest, IpcResponse};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Connect to the running SlashIt app and send a request.
/// If `wait` is true, polls up to `timeout` for the socket to appear.
pub async fn send(req: &IpcRequest, wait: bool, timeout: Duration) -> Result<IpcResponse> {
    let path = slashit_ipc::socket_path();

    let stream = if wait {
        wait_for_socket(&path, timeout).await?
    } else {
        UnixStream::connect(&path).await.map_err(|e| {
            match e.kind() {
                std::io::ErrorKind::NotFound => anyhow::anyhow!(
                    "SlashIt is not running (no socket at {})\n\
                     Start the app first, or use --wait to wait for it.",
                    path.display()
                ),
                std::io::ErrorKind::ConnectionRefused => anyhow::anyhow!(
                    "SlashIt socket exists but refused connection — the app may be shutting down.\n\
                     Try again, or use --wait to wait for a fresh start."
                ),
                std::io::ErrorKind::PermissionDenied => anyhow::anyhow!(
                    "Permission denied connecting to SlashIt socket at {}\n\
                     The socket may belong to a different user.",
                    path.display()
                ),
                _ => anyhow::anyhow!(
                    "Cannot connect to SlashIt at {}: {}",
                    path.display(),
                    e
                ),
            }
        })?
    };

    let (reader, mut writer) = stream.into_split();

    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.shutdown().await?;

    let mut buf_reader = BufReader::new(reader);
    let mut response_line = String::new();
    buf_reader
        .read_line(&mut response_line)
        .await
        .context("Failed to read response from SlashIt")?;

    if response_line.is_empty() {
        anyhow::bail!("Empty response from SlashIt -- the server may have crashed");
    }

    let response: IpcResponse =
        serde_json::from_str(&response_line).context("Failed to parse response from SlashIt")?;

    Ok(response)
}

/// Poll for the socket to appear and become connectable.
async fn wait_for_socket(
    path: &std::path::Path,
    timeout: Duration,
) -> Result<UnixStream> {
    let start = std::time::Instant::now();
    let poll_interval = Duration::from_millis(250);
    let mut printed_waiting = false;

    loop {
        match UnixStream::connect(path).await {
            Ok(stream) => {
                if printed_waiting {
                    eprintln!("Connected.");
                }
                return Ok(stream);
            }
            Err(_) if start.elapsed() < timeout => {
                if !printed_waiting {
                    eprintln!(
                        "Waiting for SlashIt to start (timeout: {}s)...",
                        timeout.as_secs()
                    );
                    printed_waiting = true;
                }
                tokio::time::sleep(poll_interval).await;
            }
            Err(e) => {
                anyhow::bail!(
                    "Timed out waiting for SlashIt after {}s ({})",
                    timeout.as_secs(),
                    e
                );
            }
        }
    }
}
