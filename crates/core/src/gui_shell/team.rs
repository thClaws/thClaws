//! Targeted Team bridge. Viewing never loads a session into the lead executor.
use crate::error::{Error, Result};
use crate::team::{Mailbox, ProtocolMessage, TeamConfig, TeamMessage};
use serde_json::{json, Value};

pub fn permission(action: &str) -> Option<&'static str> {
    match action {
        "snapshot" | "history" => Some("team.read"),
        "message" => Some("team.message"),
        "stop" => Some("team.control"),
        "manage" => Some("team.manage"),
        _ => None,
    }
}

pub fn snapshot(
    mailbox: &Mailbox,
    execution: &str,
    busy: Option<&crate::agent_activity::BusyMeta>,
) -> Value {
    let config = TeamConfig::load(&mailbox.team_dir.join("config.json")).ok();
    let mut names = vec![("lead".to_string(), "Team lead".to_string())];
    if let Some(config) = &config {
        names.extend(
            config
                .members
                .iter()
                .map(|m| (m.name.clone(), m.role.clone())),
        );
    }
    let agents: Vec<Value> = names.into_iter().map(|(name, role)| {
        let status = mailbox.read_status(&name);
        let lead = name == "lead";
        let live = if lead { !execution.is_empty() } else {
            status.as_ref().is_some_and(|s| !s.is_stale() && s.status != "stopped")
        };
        let state = if lead { if busy.is_some() { "working" } else { "idle" } }
            else if live { status.as_ref().unwrap().status.as_str() } else { "stopped" };
        let output = std::fs::read_to_string(mailbox.output_log_path(&name)).unwrap_or_default();
        let output: Vec<_> = output.lines().rev().take(80).collect::<Vec<_>>().into_iter().rev().collect();
        json!({"name": name, "role": role, "status": state, "live": live,
            "session_id": if lead { Some(execution.to_string()).filter(|s| !s.is_empty()) } else { mailbox.bound_session(&name) },
            "progress": if lead { busy.and_then(|b| b.last_progress.clone()) } else { status.and_then(|s| s.current_task) },
            "output": output})
    }).collect();
    json!({"team": config, "agents": agents})
}

/// Validate membership AND the current session binding before any control.
/// A removed/restarted member must never receive a stale card's command.
pub fn validate_target(
    mailbox: &Mailbox,
    name: &str,
    id: &str,
    execution: &str,
    require_live: bool,
) -> Result<()> {
    if id.is_empty() || !crate::team::is_valid_agent_name(name) {
        return Err(Error::Tool("Select an agent with a session first.".into()));
    }
    if name == "lead" {
        if id == execution {
            return Ok(());
        }
    } else {
        let config = TeamConfig::load(&mailbox.team_dir.join("config.json"))?;
        if !config.members.iter().any(|m| m.name == name) {
            return Err(Error::Tool(
                "This agent is no longer a team member. Refresh the team.".into(),
            ));
        }
        if mailbox.bound_session(name).as_deref() == Some(id) {
            if require_live
                && !mailbox
                    .read_status(name)
                    .is_some_and(|s| !s.is_stale() && s.status != "stopped")
            {
                return Err(Error::Tool(
                    "This agent is stopped or disconnected. Start it before sending commands."
                        .into(),
                ));
            }
            return Ok(());
        }
    }
    Err(Error::Tool(
        "Agent session changed. Refresh before sending commands.".into(),
    ))
}

pub fn send_message(mailbox: &Mailbox, name: &str, text: &str) -> Result<()> {
    let text = text.trim();
    if text.is_empty() || text.len() > 64 * 1024 {
        return Err(Error::Tool(
            "Message must contain 1–65536 bytes of text.".into(),
        ));
    }
    mailbox.write_to_mailbox(name, TeamMessage::new("user", text))
}

pub fn stop_teammate(mailbox: &Mailbox, name: &str) -> Result<()> {
    let command = serde_json::to_string(&ProtocolMessage::AbortTurn {
        from: "user".into(),
    })?;
    mailbox.write_to_mailbox(name, TeamMessage::new("user", &command))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn team_bridge_targets_and_preserves_independent_members() {
        let temp = tempfile::tempdir().unwrap();
        let mailbox = std::sync::Arc::new(Mailbox::new(temp.path().to_path_buf()));
        for input in [
            json!({"action":"create","name":"test"}),
            json!({"action":"add_member","name":"writer","prompt":"write"}),
            json!({"action":"add_member","name":"researcher","prompt":"research"}),
        ] {
            crate::team::management::manage(mailbox.clone(), input, temp.path().into())
                .await
                .unwrap();
        }
        for name in ["writer", "researcher"] {
            mailbox.init_agent(name).unwrap();
            mailbox
                .bind_session(name, &format!("session-{name}"))
                .unwrap();
            mailbox.write_status(name, "working", None).unwrap();
        }
        assert!(validate_target(
            &mailbox,
            "writer",
            "session-researcher",
            "lead-session",
            true
        )
        .is_err());
        assert!(validate_target(&mailbox, "lead", "session-writer", "lead-session", true).is_err());
        assert!(
            validate_target(&mailbox, "writer", "session-writer", "lead-session", true).is_ok()
        );
        send_message(&mailbox, "writer", "new instructions").unwrap();
        stop_teammate(&mailbox, "writer").unwrap();
        assert_eq!(mailbox.read_status("researcher").unwrap().status, "working");
        assert_eq!(mailbox.read_mailbox("writer").unwrap().len(), 2);
        assert!(mailbox.read_mailbox("researcher").unwrap().is_empty());
        assert!(mailbox.read_mailbox("lead").unwrap().is_empty());
        let result = snapshot(&mailbox, "lead-session", None);
        assert_eq!(result["agents"].as_array().unwrap().len(), 3);
        mailbox.write_status("writer", "stopped", None).unwrap();
        assert!(
            validate_target(&mailbox, "writer", "session-writer", "lead-session", true).is_err()
        );
        assert!(
            validate_target(&mailbox, "writer", "session-writer", "lead-session", false).is_ok()
        );
        assert_eq!(permission("stop"), Some("team.control"));
        assert_eq!(permission("unknown"), None);
    }
}
