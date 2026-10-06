use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const TEXT_LIMIT: usize = 16_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Human,
    Coordinator,
    Worker,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Participant {
    pub id: Uuid,
    pub role: Role,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stage {
    Proposal,
    Worker,
    Summary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Started,
    Returned,
    Interrupted,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: Uuid,
    pub participant_id: Uuid,
    pub stage: Stage,
    pub outcome: Outcome,
    pub projection: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DelegationStatus {
    Proposed,
    Approved,
    Running,
    Returned,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delegation {
    pub id: Uuid,
    pub from: Uuid,
    pub to: Uuid,
    pub proposed_request: String,
    pub explanation: String,
    pub approved_request: Option<String>,
    pub edited: bool,
    pub status: DelegationStatus,
    pub result: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    Continue,
    Redirect,
    Reject,
    Finish,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Round {
    pub goal: String,
    pub delegation: Option<Delegation>,
    pub runs: Vec<Run>,
    pub summary: Option<String>,
    pub decision: Option<Decision>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: Uuid,
    pub task_id: Uuid,
    pub revision: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub participants: Vec<Participant>,
    pub rounds: Vec<Round>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proposal {
    pub request: String,
    pub explanation: String,
}

pub fn validate_text(text: &str) -> Result<(), String> {
    if text.trim().is_empty() || text.len() > TEXT_LIMIT {
        return Err(format!("Text must contain 1–{TEXT_LIMIT} bytes"));
    }
    Ok(())
}

impl Proposal {
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > TEXT_LIMIT * 2 + 256 {
            return Err("Proposal is too large".into());
        }
        let proposal: Self =
            serde_json::from_str(text).map_err(|e| format!("Invalid proposal JSON: {e}"))?;
        validate_text(&proposal.request)?;
        validate_text(&proposal.explanation)?;
        Ok(proposal)
    }
}

impl Conversation {
    pub fn new(task_id: Uuid) -> Self {
        Self {
            id: Uuid::new_v4(),
            task_id,
            revision: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            participants: [Role::Human, Role::Coordinator, Role::Worker]
                .into_iter()
                .map(|role| Participant {
                    id: Uuid::new_v4(),
                    role,
                })
                .collect(),
            rounds: Vec::new(),
        }
    }

    pub fn participant(&self, role: Role) -> Uuid {
        self.participants
            .iter()
            .find(|p| p.role == role)
            .expect("fixed participants")
            .id
    }

    pub fn round(&self) -> Result<&Round, String> {
        self.rounds.last().ok_or("No coordination round".into())
    }
    pub fn round_mut(&mut self) -> Result<&mut Round, String> {
        self.rounds.last_mut().ok_or("No coordination round".into())
    }

    pub fn begin(&mut self, goal: String) -> Result<(), String> {
        validate_text(&goal)?;
        if self.rounds.len() >= 100 {
            return Err("Conversation round limit reached".into());
        }
        if self
            .rounds
            .last()
            .is_some_and(|r| !matches!(r.decision, Some(Decision::Continue | Decision::Redirect)))
        {
            return Err("Choose Continue or Redirect before another round".into());
        }
        self.rounds.push(Round {
            goal,
            delegation: None,
            runs: vec![],
            summary: None,
            decision: None,
        });
        Ok(())
    }

    pub fn propose(&mut self, proposal: Proposal) -> Result<(), String> {
        let from = self.participant(Role::Coordinator);
        let to = self.participant(Role::Worker);
        let round = self.round_mut()?;
        if round.delegation.is_some() {
            return Err("Proposal already recorded".into());
        }
        round.delegation = Some(Delegation {
            id: Uuid::new_v4(),
            from,
            to,
            proposed_request: proposal.request,
            explanation: proposal.explanation,
            approved_request: None,
            edited: false,
            status: DelegationStatus::Proposed,
            result: None,
        });
        Ok(())
    }

    pub fn approve(&mut self, edit: Option<String>) -> Result<(), String> {
        let d = self.round_mut()?.delegation.as_mut().ok_or("No proposal")?;
        if d.status != DelegationStatus::Proposed {
            return Err("Delegation is not Proposed".into());
        }
        let request = edit.unwrap_or_else(|| d.proposed_request.clone());
        validate_text(&request)?;
        d.edited = request != d.proposed_request;
        d.approved_request = Some(request);
        d.status = DelegationStatus::Approved;
        Ok(())
    }

    pub fn reject(&mut self) -> Result<(), String> {
        let round = self.round_mut()?;
        let d = round.delegation.as_mut().ok_or("No proposal")?;
        if d.status != DelegationStatus::Proposed {
            return Err("Delegation is not Proposed".into());
        }
        d.status = DelegationStatus::Rejected;
        round.decision = Some(Decision::Reject);
        Ok(())
    }

    pub fn next_stage(&self) -> Result<Stage, String> {
        let r = self.round()?;
        if r.decision.is_some() {
            return Err("Round already decided".into());
        }
        match &r.delegation {
            None => Ok(Stage::Proposal),
            Some(d)
                if matches!(
                    d.status,
                    DelegationStatus::Approved | DelegationStatus::Running
                ) =>
            {
                Ok(Stage::Worker)
            }
            Some(d) if d.status == DelegationStatus::Returned && r.summary.is_none() => {
                Ok(Stage::Summary)
            }
            _ => Err("Human decision required".into()),
        }
    }
}

/// Build from named fields, never by serializing a Conversation or provider transcript.
pub fn projection(
    conversation: &Conversation,
    stage: Stage,
    title: &str,
    description: &str,
) -> Result<String, String> {
    let r = conversation.round()?;
    let bounded = |s: &str| s.chars().take(4000).collect::<String>();
    let task = serde_json::json!({"title": bounded(title), "description": bounded(description)});
    let value = match stage {
        Stage::Proposal => serde_json::json!({"stage":"proposal", "task":task, "human_goal":r.goal,
            "previous_decision":conversation.rounds.iter().rev().nth(1).and_then(|r| r.decision)}),
        Stage::Worker => {
            let d = r.delegation.as_ref().ok_or("No delegation")?;
            let approved = d
                .approved_request
                .as_ref()
                .ok_or("Human approval required")?;
            if !matches!(
                d.status,
                DelegationStatus::Approved | DelegationStatus::Running
            ) {
                return Err("Worker is not approved".into());
            }
            serde_json::json!({"stage":"worker", "task":task, "approved_request":approved})
        }
        Stage::Summary => {
            let d = r.delegation.as_ref().ok_or("No delegation")?;
            serde_json::json!({"stage":"summary", "task":task, "human_goal":r.goal,
                "approved_request":d.approved_request, "human_edited":d.edited,
                "worker_result":d.result.as_ref().ok_or("No Worker result")?})
        }
    };
    serde_json::to_string(&value).map_err(|e| e.to_string())
}
