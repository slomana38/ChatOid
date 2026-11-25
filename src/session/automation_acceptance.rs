use super::*;
use std::path::Path;

fn wait(session: &Session, predicate: impl Fn(&View) -> bool) -> Result<View> {
    wait_timeout(session, Duration::from_secs(20), predicate)
}

struct AutomationPair {
    _root: tempfile::TempDir,
    _host: RunningHost,
    profiles: PathBuf,
    operator: Identity,
    agent: Identity,
    session: Option<Session>,
    connection: Connection,
    room: Uuid,
    host_pin: String,
}

impl AutomationPair {
    fn new(manual_task_approval: bool, workspace: Option<&Path>) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let operator = Identity::generate(Role::Human, "operator", "Operator")?;
        let agent = Identity::generate(Role::Agent, "automationagent", "Automation agent")?;
        let host = transport::start(&root.path().join("host"), "127.0.0.1:0".parse()?, &operator)?;

        let mut operator_connection = Connection::new(host.address.clone())?;
        let (_, ticket) = operator_connection.offer(&operator, agent.card.clone(), None)?;
        let mut agent_connection = Connection::new(host.address.clone())?;
        let request: Request = agent_connection.call(
            &agent,
            Action::Redeem {
                id: ticket.id,
                code: ticket.code,
                grant: ticket.grant,
            },
        )?;
        let room = request.terms.data.id;
        let _: Request = agent_connection.call(
            &agent,
            Action::Decide {
                decision: Decision::issue(&agent, request.terms.data, true, None)?,
            },
        )?;

        let profiles = root.path().join("profiles");
        let session = Session::new(profiles.clone(), agent.clone(), None)?;
        let mut settings = Settings {
            manual_task_approval,
            workspace_jobs: true,
            ..Settings::default()
        };
        if let Some(workspace) = workspace {
            settings.workspace = workspace.to_string_lossy().into_owned();
        }
        session.send.send(Command::Settings(settings))?;
        session
            .send
            .send(Command::Pair(pack("card", &operator.card)?))?;
        session
            .send
            .send(Command::Import(pack("host", &host.address)?))?;
        let host_pin = host.address.pin.clone();
        let connection = Connection::new(host.address.clone())?;
        let room_id = room;
        wait(&session, |view| {
            view.local.hosts.get(&host_pin).is_some_and(|link| {
                link.online
                    && link.rooms.iter().any(|room| {
                        room.id == room_id
                            && !room.group
                            && room
                                .members
                                .iter()
                                .any(|member| member.data.key == operator.id())
                    })
            })
        })?;

        Ok(Self {
            _root: root,
            _host: host,
            profiles,
            operator,
            agent,
            session: Some(session),
            connection,
            room,
            host_pin,
        })
    }

    fn session(&self) -> &Session {
        self.session.as_ref().expect("session is open")
    }

    fn reopen_session(&self) -> Result<Session> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match Session::new(self.profiles.clone(), self.agent.clone(), None) {
                Ok(session) => return Ok(session),
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                    thread::sleep(Duration::from_millis(50));
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn send(&mut self, text: &str) -> Result<Message> {
        self.connection.call(
            &self.operator,
            Action::Send {
                draft: Draft {
                    mode: None,
                    id: Uuid::new_v4(),
                    room: self.room,
                    text: text.into(),
                    reply: None,
                    invitation: None,
                    attachment: None,
                },
            },
        )
    }
}

#[test]
fn loop_is_private_pending_cancellable_and_persistent_after_restart() -> Result<()> {
    let mut pair = AutomationPair::new(true, None)?;
    let command = pair.send("/loop 10s Review the project summary")?;
    let created = wait(pair.session(), |view| {
        view.local
            .loops
            .iter()
            .any(|task| task.source == command.seq && task.active && task.interval_seconds == 10)
    })?;
    let loop_task = created
        .local
        .loops
        .iter()
        .find(|task| task.source == command.seq)
        .expect("loop was created")
        .clone();

    let due = wait(pair.session(), |view| {
        view.local.jobs.iter().any(|job| {
            job.automation == Some(AutomationRef::Loop(loop_task.id))
                && job.status == WorkspaceJobStatus::Pending
        })
    })?;
    let job = due
        .local
        .jobs
        .iter()
        .find(|job| job.automation == Some(AutomationRef::Loop(loop_task.id)))
        .expect("loop job was queued");
    assert_eq!(job.actor, pair.operator.id());
    assert_eq!(job.selection.room, pair.room);
    assert_eq!(job.selection.host, pair.host_pin);
    let room = due.local.hosts[&pair.host_pin]
        .rooms
        .iter()
        .find(|room| room.id == pair.room)
        .expect("paired direct room exists");
    assert!(!room.group);
    assert!(
        room.members
            .iter()
            .any(|member| member.data.key == pair.operator.id())
    );

    pair.send(&format!("/loop stop {}", loop_task.id))?;
    let stopped = wait(pair.session(), |view| {
        view.local
            .loops
            .iter()
            .any(|task| task.id == loop_task.id && !task.active)
            && view.local.jobs.iter().any(|job| {
                job.automation == Some(AutomationRef::Loop(loop_task.id))
                    && job.status == WorkspaceJobStatus::Cancelled
            })
    })?;
    let job_count = stopped.local.jobs.len();

    drop(pair.session.take());
    pair.session = Some(pair.reopen_session()?);
    let restored = wait(pair.session(), |view| {
        view.local
            .loops
            .iter()
            .any(|task| task.id == loop_task.id && !task.active)
            && view.local.jobs.len() == job_count
    })?;
    assert_eq!(restored.local.jobs.len(), job_count);
    assert_eq!(
        restored
            .local
            .jobs
            .iter()
            .filter(|job| job.automation == Some(AutomationRef::Loop(loop_task.id)))
            .count(),
        1
    );
    Ok(())
}

#[test]
fn goal_stop_cancels_pending_work_and_resume_queues_the_same_goal() -> Result<()> {
    let mut pair = AutomationPair::new(true, None)?;
    let command = pair.send("/goal Create and maintain a concise project summary")?;
    let queued = wait(pair.session(), |view| {
        view.local
            .goals
            .iter()
            .any(|goal| goal.source == command.seq)
            && view.local.jobs.iter().any(|job| {
                job.automation
                    .is_some_and(|reference| matches!(reference, AutomationRef::Goal(_)))
                    && job.status == WorkspaceJobStatus::Pending
            })
    })?;
    let goal = queued
        .local
        .goals
        .iter()
        .find(|goal| goal.source == command.seq)
        .expect("goal was created")
        .clone();
    let initial_job = queued
        .local
        .jobs
        .iter()
        .find(|job| job.automation == Some(AutomationRef::Goal(goal.id)))
        .expect("goal job was queued");
    assert_eq!(initial_job.actor, pair.operator.id());
    assert_eq!(initial_job.selection.room, pair.room);
    assert_eq!(initial_job.selection.host, pair.host_pin);
    assert!(
        !queued.local.hosts[&pair.host_pin]
            .rooms
            .iter()
            .find(|room| room.id == pair.room)
            .expect("paired room exists")
            .group
    );

    pair.send("/goal stop")?;
    let stopped = wait(pair.session(), |view| {
        view.local
            .goals
            .iter()
            .any(|item| item.id == goal.id && item.status == automation::GoalState::Stopped)
            && view.local.jobs.iter().any(|job| {
                job.automation == Some(AutomationRef::Goal(goal.id))
                    && job.status == WorkspaceJobStatus::Cancelled
            })
    })?;
    assert_eq!(stopped.local.jobs.len(), 1);

    pair.send("/goal resume")?;
    let resumed = wait(pair.session(), |view| {
        view.local
            .goals
            .iter()
            .any(|item| item.id == goal.id && item.status == automation::GoalState::Active)
            && view.local.jobs.iter().any(|job| {
                job.automation == Some(AutomationRef::Goal(goal.id))
                    && job.status == WorkspaceJobStatus::Pending
                    && job.id != initial_job.id
            })
    })?;
    assert_eq!(resumed.local.jobs.len(), 2);
    assert_eq!(
        resumed
            .local
            .jobs
            .iter()
            .filter(|job| job.automation == Some(AutomationRef::Goal(goal.id)))
            .filter(|job| job.status == WorkspaceJobStatus::Cancelled)
            .count(),
        1
    );
    assert_eq!(
        resumed
            .local
            .jobs
            .iter()
            .filter(|job| job.automation == Some(AutomationRef::Goal(goal.id)))
            .filter(|job| job.status == WorkspaceJobStatus::Pending)
            .count(),
        1
    );
    Ok(())
}

#[test]
fn inline_retry_goal_uses_only_the_operators_quoted_capture_request() -> Result<()> {
    let mut pair = AutomationPair::new(true, None)?;
    let original = pair.send("Masaüstünün ekran görüntüsünü gönder.")?;
    let command: Message = pair.connection.call(
        &pair.operator,
        Action::Send {
            draft: Draft {
                mode: None,
                id: Uuid::new_v4(),
                room: pair.room,
                text: "bir daha dene /goal".into(),
                reply: Some(original.seq),
                invitation: None,
                attachment: None,
            },
        },
    )?;
    let queued = wait(pair.session(), |view| {
        view.local
            .jobs
            .iter()
            .any(|job| job.source == command.seq && job.automation.is_some())
    })?;
    let job = queued
        .local
        .jobs
        .iter()
        .find(|job| job.source == command.seq && job.automation.is_some())
        .expect("inline goal job");
    let link = &queued.local.hosts[&pair.host_pin];
    assert!(requested_desktop_capture(&operator_request_text(link, job)));
    let other = Identity::generate(Role::Human, "outsider", "Outsider")?;
    let mut forged = link.clone();
    forged
        .messages
        .get_mut(&command.seq)
        .expect("retry")
        .reply
        .as_mut()
        .expect("quote")
        .author = other.card;
    assert!(
        !requested_desktop_capture(&operator_request_text(&forged, job)),
        "another person's quote must not authorize desktop capture"
    );
    Ok(())
}

#[test]
#[ignore = "requires a usable local Codex login and installed Codex app-server"]
fn real_goal_creates_workspace_file_and_marks_it_complete() -> Result<()> {
    let root = tempfile::tempdir()?;
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace)?;
    let mut pair = AutomationPair::new(false, Some(&workspace))?;
    let file_name = format!("goal-{}.txt", Uuid::new_v4());
    let exact_text = "verified persistent goal output\n";
    pair.send(&format!(
        "/goal Create {file_name} in the workspace with exactly this content: {exact_text:?}. Verify the file, then call chatoid_goal_progress with status complete and a short factual summary."
    ))?;

    let complete = wait_timeout(pair.session(), Duration::from_secs(120), |view| {
        view.local
            .goals
            .iter()
            .any(|goal| goal.status == automation::GoalState::Complete)
    })?;
    assert_eq!(fs::read_to_string(workspace.join(file_name))?, exact_text);
    assert!(
        complete
            .local
            .jobs
            .iter()
            .any(|job| job.automation.is_some_and(|reference| {
                matches!(reference, AutomationRef::Goal(_))
                    && job.status == WorkspaceJobStatus::Complete
            }))
    );
    Ok(())
}

#[test]
#[cfg(windows)]
#[ignore = "requires local Codex login and the interactive Windows desktop"]
fn normal_message_delivers_desktop_image_without_job_or_capture_approval() -> Result<()> {
    let mut pair = AutomationPair::new(false, None)?;
    let source = pair.send("Masaüstünün ekran görüntüsünü at.")?;
    let completed = wait_timeout(pair.session(), Duration::from_secs(120), |view| {
        view.local.hosts.get(&pair.host_pin).is_some_and(|link| {
            link.messages.values().any(|message| {
                message.author.data.key == pair.agent.id()
                    && message
                        .reply
                        .as_ref()
                        .is_some_and(|quote| quote.id == source.seq)
                    && message.attachment.is_some()
            })
        })
    })?;
    assert!(completed.local.task_approval.is_none());
    let message = completed.local.hosts[&pair.host_pin]
        .messages
        .values()
        .find(|message| {
            message.attachment.is_some()
                && message
                    .reply
                    .as_ref()
                    .is_some_and(|quote| quote.id == source.seq)
        })
        .expect("image reply");
    let mut data = Vec::new();
    pair.connection
        .download_attachment(&pair.operator, message, &mut data, || Ok(()))?;
    let image = image::load_from_memory(&data)?;
    assert!(image.width() > 0 && image.height() > 0);
    assert!(
        !pair
            .profiles
            .join(pair.agent.id())
            .join("codex-tasks/auth.json")
            .exists()
    );
    Ok(())
}

#[test]
#[cfg(windows)]
#[ignore = "requires local Codex login and the interactive Windows desktop"]
fn real_goal_shows_desktop_posts_image_then_writes_utf8_file() -> Result<()> {
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    let filename = format!("ChatOid-test-{}.txt", Uuid::new_v4());
    let path = agent::desktop_tools::desktop_folder()?.join(&filename);
    ensure!(!path.exists(), "Test yolu zaten mevcut");
    let _cleanup = Cleanup(path.clone());
    let mut pair = AutomationPair::new(false, None)?;
    let source = pair.send(&format!(
        "/goal Masaüstüne gel, düz masaüstümü görmek istiyorum. Ardından ekran görüntüsünü al ve bu sohbete at. En son masaüstüne {filename} adlı, yalnızca “我爱你” yazan bir .txt dosyası oluştur."
    ))?;
    let done = wait_timeout(pair.session(), Duration::from_secs(115), |view| {
        view.local
            .goals
            .iter()
            .any(|goal| goal.source == source.seq && goal.status != automation::GoalState::Active)
    })?;
    let goal = done
        .local
        .goals
        .iter()
        .find(|goal| goal.source == source.seq)
        .unwrap();
    assert_eq!(
        goal.status,
        automation::GoalState::Complete,
        "{}",
        goal.progress
    );
    assert_eq!(fs::read(&path)?, "我爱你".as_bytes());
    let job = done
        .local
        .jobs
        .iter()
        .find(|job| job.source == source.seq && job.status == WorkspaceJobStatus::Complete)
        .context("Görev sonucu eksik")?
        .clone();
    let complete = wait(pair.session(), |view| {
        view.local.hosts[&pair.host_pin]
            .messages
            .values()
            .any(|message| message.client_id == job.id)
    })?;
    let messages = &complete.local.hosts[&pair.host_pin].messages;
    let screenshot = messages
        .values()
        .find(|message| message.client_id == desktop_delivery_id(&job))
        .context("Host onaylı ekran görüntüsü eksik")?;
    let finished = messages
        .values()
        .find(|message| message.client_id == job.id)
        .unwrap();
    assert!(screenshot.seq < finished.seq);
    let mut bytes = Vec::new();
    pair.connection
        .download_attachment(&pair.operator, screenshot, &mut bytes, || Ok(()))?;
    let image = image::load_from_memory(&bytes)?;
    assert!(image.width() > 0 && image.height() > 0);
    assert!(complete.local.task_approval.is_none() && complete.local.desktop_request.is_none());
    Ok(())
}

fn wait_timeout(
    session: &Session,
    timeout: Duration,
    predicate: impl Fn(&View) -> bool,
) -> Result<View> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let view = session
            .receive
            .recv_timeout(remaining)
            .context("automation acceptance timed out")?;
        if predicate(&view) {
            return Ok(view);
        }
    }
}

#[test]
fn committed_mode_uses_clean_text_and_withdrawn_goal_never_reaches_agent() -> Result<()> {
    let mut pair = AutomationPair::new(true, None)?;
    let cancelled: Message = pair.connection.call(
        &pair.operator,
        Action::Send {
            draft: Draft {
                id: Uuid::new_v4(),
                room: pair.room,
                text: "Bu hedef çalıştırılmamalı".into(),
                reply: None,
                invitation: None,
                mode: Some(MessageMode::Goal),
                attachment: None,
            },
        },
    )?;
    pair.connection.call::<Value>(
        &pair.operator,
        Action::WithdrawMessage {
            room: pair.room,
            seq: cancelled.seq,
        },
    )?;
    let withdrawn = wait(pair.session(), |view| {
        view.local.hosts[&pair.host_pin]
            .messages
            .get(&cancelled.seq)
            .is_some_and(|m| m.withdrawn)
    })?;
    assert!(
        withdrawn
            .local
            .jobs
            .iter()
            .all(|job| job.source != cancelled.seq)
    );
    assert!(
        withdrawn
            .local
            .goals
            .iter()
            .all(|goal| goal.source != cancelled.seq)
    );
    let committed: Message = pair.connection.call(
        &pair.operator,
        Action::Send {
            draft: Draft {
                id: Uuid::new_v4(),
                room: pair.room,
                text: "Temiz hedef yönergesi".into(),
                reply: None,
                invitation: None,
                mode: Some(MessageMode::Goal),
                attachment: None,
            },
        },
    )?;
    let active = wait(pair.session(), |view| {
        view.local
            .goals
            .iter()
            .any(|goal| goal.source == committed.seq)
    })?;
    let goal = active
        .local
        .goals
        .iter()
        .find(|goal| goal.source == committed.seq)
        .unwrap();
    assert_eq!(goal.objective, "Temiz hedef yönergesi");
    let literal: Message = pair.connection.call(
        &pair.operator,
        Action::Send {
            draft: Draft {
                id: Uuid::new_v4(),
                room: pair.room,
                text: "/goal bu düz metindir".into(),
                reply: None,
                invitation: None,
                mode: Some(MessageMode::Plain),
                attachment: None,
            },
        },
    )?;
    let done = wait(pair.session(), |view| {
        view.local.jobs.iter().any(|job| job.source == literal.seq)
    })?;
    assert_eq!(done.local.goals.len(), 1);
    assert!(
        done.local
            .jobs
            .iter()
            .find(|job| job.source == literal.seq)
            .unwrap()
            .automation
            .is_none()
    );
    Ok(())
}
