// SPDX-License-Identifier: MPL-2.0
//! Project Conversation settings and the durable composer draft.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Largest draft text kept on disk, in bytes. A draft is an unsent message, so
/// it is bounded like one: longer text is refused rather than silently cut.
pub const DRAFT_LIMIT: usize = 64 * 1024;

/// The key that sends a Conversation message. Enter inserts a newline in the
/// default mode, so a multi-line message is never sent by accident.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendShortcut {
    /// Enter inserts a newline; Ctrl+Enter (Cmd+Enter on macOS) sends.
    #[default]
    ModifierEnter,
    /// Enter sends; Shift+Enter inserts a newline.
    Enter,
}

/// The `[conversations]` section of `config.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConversationsConfig {
    pub send_shortcut: SendShortcut,
}

/// The composer draft of one Project. It is machine-local and is never part of
/// the Conversation history, a bundle, or the Project's repository.
///
/// `seq` orders the writes made by the composer: the store keeps only the
/// highest `seq`, so an older asynchronous save cannot restore older text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationDraft {
    pub seq: u64,
    pub text: String,
    /// Set from the moment a send starts until the composer has confirmed,
    /// from the history, whether the Human message was saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_send: Option<PendingSend>,
}

/// A send that may have saved its Human message. The message is recorded here
/// because the composer text can change while the send is in flight.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingSend {
    pub conversation_id: Uuid,
    /// Entries are append-only, so the message is saved exactly when the
    /// history has a Human message with this text after this many entries.
    pub entries_before: usize,
    pub message: String,
}

impl ConversationDraft {
    /// Refuse a draft that cannot be kept whole.
    pub fn validate(&self) -> Result<(), String> {
        if self.text.len() > DRAFT_LIMIT {
            return Err(format!("A draft can hold at most {DRAFT_LIMIT} bytes"));
        }
        if let Some(pending) = &self.pending_send {
            if pending.message.len() > DRAFT_LIMIT {
                return Err(format!(
                    "A pending message can hold at most {DRAFT_LIMIT} bytes"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_inserts_a_newline_unless_the_setting_says_otherwise() {
        assert_eq!(
            ConversationsConfig::default().send_shortcut,
            SendShortcut::ModifierEnter
        );
        let parsed: ConversationsConfig = toml::from_str("send_shortcut = \"enter\"\n").unwrap();
        assert_eq!(parsed.send_shortcut, SendShortcut::Enter);
        let absent: ConversationsConfig = toml::from_str("").unwrap();
        assert_eq!(absent.send_shortcut, SendShortcut::ModifierEnter);
    }

    #[test]
    fn a_draft_round_trips_and_an_oversized_one_is_refused() {
        let draft = ConversationDraft {
            seq: 7,
            text: "hello".into(),
            pending_send: None,
        };
        let json = serde_json::to_string(&draft).unwrap();
        assert!(!json.contains("pendingSend"), "{json}");
        let back: ConversationDraft = serde_json::from_str(&json).unwrap();
        assert_eq!(back, draft);
        let big = ConversationDraft {
            seq: 1,
            text: "x".repeat(DRAFT_LIMIT + 1),
            pending_send: None,
        };
        assert!(big.validate().is_err());
        assert!(draft.validate().is_ok());
    }
}
