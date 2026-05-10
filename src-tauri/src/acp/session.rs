use super::client::AcpClient;
use anyhow::Result;

pub struct AcpSession {
    pub client: AcpClient,
    pub session_id: String,
}

impl AcpSession {
    pub async fn new(client: AcpClient, name: String) -> Result<Self> {
        let session_id = client.create_session(name).await?;
        Ok(Self { client, session_id })
    }

    pub async fn send_prompt(&self, prompt: String) -> Result<()> {
        self.client
            .send_prompt(self.session_id.clone(), prompt)
            .await
    }

    pub async fn stop(&self) -> Result<()> {
        self.client.stop(self.session_id.clone()).await
    }
}
