use super::protocol::*;
use anyhow::Result;
use std::io::{BufRead, BufReader};
use std::process::Child;
use tokio::sync::mpsc;

pub struct AcpStream {
    receiver: mpsc::UnboundedReceiver<AcpNotification>,
}

impl AcpStream {
    pub fn new(mut child: Child) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();

        std::thread::spawn(move || {
            if let Some(stdout) = child.stdout.as_mut() {
                let reader = BufReader::new(stdout);
                for line in reader.lines().flatten() {
                    if let Ok(notification) = serde_json::from_str::<AcpNotification>(&line) {
                        let _ = sender.send(notification);
                    }
                }
            }
        });

        Self { receiver }
    }

    pub async fn recv(&mut self) -> Option<AcpNotification> {
        self.receiver.recv().await
    }
}
