use super::*;

#[test]
#[ignore = "requires an authenticated local Codex CLI and a working workspace sandbox"]
fn real_codex_exports_a_locally_approved_task_file() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let operator = Identity::generate(Role::Human, "deniz", "Deniz")?;
    let agent = Identity::generate(Role::Agent, "poyraz", "Poyraz")?;
    let running = transport::start(&temp.path().join("host"), "127.0.0.1:0".parse()?, &operator)?;
    let mut connection = Connection::new(running.address.clone())?;
    let (_, ticket) = connection.offer(&operator, agent.card.clone(), None)?;
    let mut guest = Connection::new(running.address.clone())?;
    let request: Request = guest.call(
        &agent,
        Action::Redeem {
            id: ticket.id,
            code: ticket.code,
            grant: ticket.grant,
        },
    )?;
    let room = request.terms.data.id;
    let _: Request = guest.call(
        &agent,
        Action::Decide {
            decision: Decision::issue(&agent, request.terms.data, true, None)?,
        },
    )?;

    let workspace = temp.path().join("work");
    fs::create_dir(&workspace)?;
    let profiles = temp.path().join("profiles");
    let session = Session::new(profiles.clone(), agent.clone(), None)?;
    session.send.send(Command::Settings(Settings {
        manual_task_approval: true,
        workspace: workspace.to_string_lossy().into_owned(),
        workspace_jobs: true,
        ..Settings::default()
    }))?;
    session
        .send
        .send(Command::Pair(pack("card", &operator.card)?))?;
    session
        .send
        .send(Command::Import(pack("host", &running.address)?))?;
    let source: Message = connection.call(&operator, Action::Send { draft: Draft {
 mode: None,
        id: Uuid::new_v4(), room,
        text: "Create report.txt in the current workspace containing the ASCII line CHAT-EXPORT-5609, a newline, and exactly 1,200,000 uppercase X characters. Use a short local command to generate the repetitions. Then call chatoid_share_file with the relative path report.txt to attach it to the ChatOid reply. Summarize briefly.".into(),
        reply: None, invitation: None, attachment: None,
    } })?;
    let deadline = Instant::now() + Duration::from_secs(100);
    let mut start_approved = false;
    let mut file_approval = None;
    let reply = loop {
        let view = session
            .receive
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
        let Some(job) = view.local.jobs.iter().find(|job| job.source == source.seq) else {
            continue;
        };
        if job.status == WorkspaceJobStatus::Pending && !start_approved {
            assert!(!workspace.join("report.txt").exists());
            session.send.send(Command::DecideWorkspaceJob(
                job.id,
                WorkspaceDecision::Approve {
                    web_research: false,
                    network: false,
                    delegate: false,
                    model: None,
                    plan_only: false,
                    skills: Vec::new(),
                    compact_before: false,
                    fork: false,
                    mcp: false,
                },
            ))?;
            start_approved = true;
        }
        if let Some(approval) = &view.local.task_approval
            && file_approval.is_none()
        {
            assert!(approval.file);
            assert_eq!(approval.title, "report.txt");
            let state: Sync = connection.call(
                &operator,
                Action::Sync {
                    after: 0,
                    grants: vec![],
                    read: Default::default(),
                    avatar: None,
                },
            )?;
            assert!(
                !state
                    .messages
                    .iter()
                    .any(|message| message.client_id == job.id)
            );
            session
                .send
                .send(Command::DecideTaskAction(approval.id, true))?;
            file_approval = Some(approval.id);
        }
        if let WorkspaceJobStatus::Failed(error) = &job.status {
            anyhow::bail!("{error}");
        }
        let link = &view.local.hosts[&running.address.pin];
        if let Some(error) = link
            .queue
            .iter()
            .find(|entry| entry.draft.id == job.id)
            .and_then(|entry| entry.error.as_deref())
        {
            anyhow::bail!("File was not delivered: {error}");
        }
        if let Some(reply) = link
            .messages
            .values()
            .find(|message| message.client_id == job.id)
        {
            break reply.clone();
        }
    };
    assert!(start_approved && file_approval.is_some(), "{}", reply.text);
    assert_eq!(reply.reply.as_ref().context("quoted reply")?.id, source.seq);
    let attachment = reply.attachment.as_ref().context("no task attachment")?;
    assert!(attachment.binary && attachment.bytes > MAX_ATTACHMENT);
    assert_eq!(attachment.name, "report.txt");
    let mut bytes = Vec::new();
    connection.download_attachment(&operator, &reply, &mut bytes, || Ok(()))?;
    assert_eq!(bytes, fs::read(workspace.join("report.txt"))?);
    assert!(String::from_utf8_lossy(&bytes[..30]).contains("CHAT-EXPORT-5609"));
    assert_eq!(
        bytes.iter().filter(|byte| **byte == b'X').count(),
        1_200_001
    );
    assert!(
        !profiles
            .join(agent.id())
            .join("task-outputs")
            .join(reply.client_id.to_string())
            .exists()
    );
    Ok(())
}
