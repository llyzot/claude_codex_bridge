use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};

macro_rules! wire_enum {
    ($name:ident { $($variant:ident),* }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),* }
    };
}
wire_enum!(EventType {
    TaskRequest,
    TaskReply,
    CompletionNotice,
    RetrySignal,
    SystemSignal,
    BarrierRelease
});
wire_enum!(EventStatus {
    Created,
    Queued,
    Delivering,
    Consumed,
    Superseded,
    Abandoned
});
wire_enum!(LeaseState {
    Acquired,
    Released,
    Expired,
    Orphaned
});
impl EventStatus {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Consumed | Self::Superseded | Self::Abandoned)
    }
    pub fn claimable(self) -> bool {
        matches!(self, Self::Created | Self::Queued)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub schema_version: u32,
    pub record_type: String,
    pub inbound_event_id: String,
    pub agent_name: String,
    pub event_type: EventType,
    pub message_id: String,
    pub attempt_id: Option<String>,
    pub payload_ref: Option<String>,
    #[serde(default)]
    pub priority: u64,
    pub status: EventStatus,
    #[serde(default)]
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}
impl Event {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1 && self.record_type == "inbound_event_record",
            "invalid inbound schema"
        );
        ensure!(
            !self.inbound_event_id.is_empty() && !self.message_id.is_empty(),
            "empty event identity"
        );
        validate_agent(&self.agent_name)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Lease {
    pub schema_version: u32,
    pub record_type: String,
    pub agent_name: String,
    pub inbound_event_id: String,
    pub lease_version: u64,
    pub acquired_at: String,
    pub last_progress_at: Option<String>,
    pub expires_at: Option<String>,
    pub lease_state: LeaseState,
}
impl Lease {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1 && self.record_type == "delivery_lease",
            "invalid lease schema"
        );
        ensure!(
            !self.inbound_event_id.is_empty(),
            "empty lease event identity"
        );
        validate_agent(&self.agent_name)
    }
}

pub fn validate_agent(name: &str) -> Result<()> {
    ensure!(!name.is_empty() && name.len() <= 32, "invalid agent name");
    ensure!(
        name.as_bytes()[0].is_ascii_lowercase(),
        "invalid agent name"
    );
    ensure!(
        name.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'),
        "invalid agent name"
    );
    ensure!(
        ![
            "all",
            "from",
            "user",
            "system",
            "ask",
            "cancel",
            "clear",
            "pend",
            "ping",
            "watch",
            "kill",
            "ps",
            "logs",
            "doctor",
            "config",
            "cmd",
            "version",
            "update",
            "help",
            "manual",
            "email",
            "maintenance-heartbeat"
        ]
        .contains(&name),
        "actor does not own a mailbox"
    );
    Ok(())
}
