use super::*;

/// UI actions operate on the same mailbox as native team tools. Serialize UI
/// mutations, including the spawn probe, to prevent duplicate button requests.
pub async fn manage(mailbox: Arc<Mailbox>, input: Value, cwd: PathBuf) -> Result<String> {
    static ACTIONS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = ACTIONS.lock().await;
    let action = crate::tools::req_str(&input, "action")?;
    let path = mailbox.team_dir.join("config.json");
    if action == "create" {
        if path.exists() {
            return Err(Error::Tool(
                "A team already exists. Edit the current team instead.".into(),
            ));
        }
        let name = required_text(&input, "name")?;
        TeamConfig {
            name,
            description: Some(text(&input, "description")),
            created_at: now_secs(),
            lead_agent_id: "lead".into(),
            members: vec![],
            agents: vec![],
        }
        .save(&path)?;
        return Ok("Team created. Add teammates, then start them when ready.".into());
    }
    let mut config = TeamConfig::load(&path)?;
    if action == "edit_team" {
        config.name = required_text(&input, "name")?;
        config.description = Some(text(&input, "description"));
        config.save(&path)?;
        return Ok("Team updated.".into());
    }
    if action == "shutdown_all" {
        for member in &config.members {
            if live(&mailbox, &member.name) {
                mailbox.write_to_mailbox(
                    &member.name,
                    TeamMessage::new("user", &make_shutdown_request("user")),
                )?;
            }
        }
        return Ok("Shutdown requested. Members with unfinished tasks may refuse; check their status and log.".into());
    }
    let name = required_text(&input, "name")?;
    if name == "lead" || !is_valid_agent_name(&name) {
        return Err(Error::Tool(
            "Use a teammate name with 1–64 letters, digits, _ or -; lead is reserved.".into(),
        ));
    }
    let index = config.members.iter().position(|m| m.name == name);
    if action == "add_member" {
        if index.is_some() || mailbox.bound_session(&name).is_some() {
            return Err(Error::Tool("This name is already used, including by an archived member. Choose a new name to preserve its history.".into()));
        }
        config.members.push(TeamMember {
            name,
            role: text(&input, "role"),
            prompt: required_text(&input, "prompt")?,
            color: None,
            cwd: None,
            is_active: false,
            tmux_pane_id: None,
            isolation: None,
        });
        config.save(&path)?;
        return Ok("Teammate added. Start it to run its instructions.".into());
    }
    let index = index.ok_or_else(|| {
        Error::Tool("Teammate is no longer in this team. Refresh and try again.".into())
    })?;
    match action {
        "edit_member" => {
            if live(&mailbox, &name) {
                return Err(Error::Tool(
                    "Shut down this teammate before editing its role and instructions.".into(),
                ));
            }
            config.members[index].role = text(&input, "role");
            config.members[index].prompt = required_text(&input, "prompt")?;
            config.save(&path)?;
            Ok("Role and instructions updated for the next start.".into())
        }
        "remove_member" => {
            if live(&mailbox, &name) {
                return Err(Error::Tool(
                    "Shut down this teammate before removing it.".into(),
                ));
            }
            if mailbox
                .task_queue()
                .list(None)?
                .iter()
                .any(|t| t.owner.as_deref() == Some(&name) && t.status != TaskStatus::Completed)
            {
                return Err(Error::Tool("This teammate still owns unfinished tasks. Ask the lead to resolve or reassign them first.".into()));
            }
            config.members.remove(index);
            config.save(&path)?;
            Ok("Member removed. Sessions, logs and completed tasks are preserved.".into())
        }
        "start_member" => {
            if live(&mailbox, &name) {
                return Err(Error::Tool(
                    "This teammate is already running or starting.".into(),
                ));
            }
            let member = &config.members[index];
            let prompt = format!(
                "[Team: {}]\n[Role: {}]\n{}",
                config.name, member.role, member.prompt
            );
            SpawnTeammateTool {
                mailbox,
                my_name: "lead".into(),
            }
            .call(json!({"name":name,"prompt":prompt,"cwd":cwd}))
            .await
        }
        "shutdown_member" => {
            if !live(&mailbox, &name) {
                return Err(Error::Tool("This teammate is not running.".into()));
            }
            mailbox.write_to_mailbox(
                &name,
                TeamMessage::new("user", &make_shutdown_request("user")),
            )?;
            Ok("Shutdown requested. It may refuse while it owns unfinished tasks; check status and log.".into())
        }
        "abort_member" => {
            if !live(&mailbox, &name) {
                return Err(Error::Tool("This teammate is not running.".into()));
            }
            let command = serde_json::to_string(&ProtocolMessage::AbortTurn {
                from: "user".into(),
            })?;
            mailbox.write_to_mailbox(&name, TeamMessage::new("user", &command))?;
            Ok("Stop requested for the current turn. The teammate remains available.".into())
        }
        _ => Err(Error::Tool("Unknown team action".into())),
    }
}

pub fn live(mailbox: &Mailbox, name: &str) -> bool {
    mailbox
        .read_status(name)
        .is_some_and(|s| s.status != "stopped" && !s.is_stale())
}
fn text(input: &Value, key: &str) -> String {
    input[key].as_str().unwrap_or("").trim().to_string()
}
fn required_text(input: &Value, key: &str) -> Result<String> {
    let value = text(input, key);
    if value.is_empty() {
        Err(Error::Tool(format!("{key} is required")))
    } else {
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn membership_management_preserves_history_and_rejects_live_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let mb = Arc::new(Mailbox::new(tmp.path().join("team")));
        let cwd = tmp.path().to_path_buf();
        manage(
            mb.clone(),
            json!({"action":"create","name":"demo"}),
            cwd.clone(),
        )
        .await
        .unwrap();
        assert!(manage(
            mb.clone(),
            json!({"action":"create","name":"overwrite"}),
            cwd.clone()
        )
        .await
        .is_err());
        assert!(manage(
            mb.clone(),
            json!({"action":"add_member","name":"../bad","prompt":"work"}),
            cwd.clone()
        )
        .await
        .is_err());
        manage(
            mb.clone(),
            json!({"action":"add_member","name":"writer","role":"Writer","prompt":"Draft"}),
            cwd.clone(),
        )
        .await
        .unwrap();
        mb.write_status("writer", "idle", None).unwrap();
        assert!(manage(
            mb.clone(),
            json!({"action":"remove_member","name":"writer"}),
            cwd.clone()
        )
        .await
        .is_err());
        manage(
            mb.clone(),
            json!({"action":"shutdown_member","name":"writer"}),
            cwd.clone(),
        )
        .await
        .unwrap();
        assert!(matches!(
            parse_protocol_message(mb.read_unread("writer").unwrap()[0].content()),
            Some(ProtocolMessage::ShutdownRequest { .. })
        ));
        assert!(manage(
            mb.clone(),
            json!({"action":"start_member","name":"writer"}),
            cwd.clone()
        )
        .await
        .is_err());
        assert!(manage(
            mb.clone(),
            json!({"action":"edit_member","name":"writer","prompt":"new"}),
            cwd.clone()
        )
        .await
        .is_err());
        mb.write_status("writer", "stopped", None).unwrap();
        let task = mb
            .task_queue()
            .create("unfinished", "", &[], Some("writer"))
            .unwrap();
        assert!(manage(
            mb.clone(),
            json!({"action":"remove_member","name":"writer"}),
            cwd.clone()
        )
        .await
        .is_err());
        mb.task_queue().claim(&task.id, "writer").unwrap();
        mb.task_queue().complete(&task.id, "writer").unwrap();
        mb.bind_session("writer", "saved-session").unwrap();
        std::fs::write(mb.output_log_path("writer"), "saved history").unwrap();
        manage(
            mb.clone(),
            json!({"action":"remove_member","name":"writer"}),
            cwd,
        )
        .await
        .unwrap();
        assert!(TeamConfig::load(&mb.team_dir.join("config.json"))
            .unwrap()
            .members
            .is_empty());
        assert_eq!(mb.bound_session("writer").as_deref(), Some("saved-session"));
        assert_eq!(
            std::fs::read_to_string(mb.output_log_path("writer")).unwrap(),
            "saved history"
        );
    }
}
