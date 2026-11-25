use super::*;
use serde_json::json;
use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
};

fn native_fixture_tool(tools: &Value, namespace: Option<&str>) -> Option<(String, Option<String>)> {
    for tool in tools.as_array()? {
        if tool["type"] == "namespace" {
            if let Some(found) = native_fixture_tool(&tool["tools"], tool["name"].as_str()) {
                return Some(found);
            }
        } else if let Some(name) = tool["name"]
            .as_str()
            .filter(|name| name.ends_with("fixture_echo"))
        {
            return Some((name.into(), namespace.map(str::to_owned)));
        }
    }
    None
}

#[cfg(windows)]
fn assert_fixture_process_stopped(pid_file: &std::path::Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let pid: u32 = fs::read_to_string(pid_file)?.trim().parse()?;
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let output = std::process::Command::new("tasklist.exe")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .creation_flags(0x08000000)
            .output()?;
        ensure!(output.status.success(), "Could not inspect fixture process");
        if !String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\"")) {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "Native MCP fixture remained alive after task completion"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
#[ignore = "requires local Codex App Server; uses a simulated provider on loopback"]
fn quoted_operator_reply_reaches_the_waiting_codex_tool() -> Result<()> {
    exercise_quoted_reply(false, false, false, false, false)
}

#[test]
#[ignore = "requires local Codex App Server; uses a simulated provider on loopback"]
fn planning_answers_native_question_and_rejects_file_sharing() -> Result<()> {
    exercise_quoted_reply(true, false, false, false, false)
}

#[test]
#[ignore = "requires local Codex App Server; uses a simulated provider on loopback"]
fn fork_uses_the_quoted_turn_and_keeps_parent_history_separate() -> Result<()> {
    exercise_quoted_reply(false, true, false, false, false)
}

#[test]
#[ignore = "requires local Codex App Server and Python; uses a simulated provider and MCP server"]
fn chat_task_gets_an_approved_mcp_result_and_sends_it_to_the_operator() -> Result<()> {
    exercise_quoted_reply(false, false, true, false, false)
}

#[test]
#[ignore = "requires local Codex App Server and Python; uses a simulated provider and MCP server"]
fn cancelling_native_mcp_stops_the_owned_server_process() -> Result<()> {
    exercise_quoted_reply(false, false, true, false, true)
}

#[test]
#[ignore = "requires local Codex App Server; uses a simulated provider on loopback"]
fn interrupted_task_resumes_its_saved_history_after_agent_restart() -> Result<()> {
    exercise_quoted_reply(false, false, false, true, false)
}

fn exercise_quoted_reply(
    plan_only: bool,
    fork_mode: bool,
    mcp_mode: bool,
    recovery_mode: bool,
    mcp_cancel: bool,
) -> Result<()> {
    let delegate = !plan_only && !fork_mode && !mcp_mode && !recovery_mode;
    let temp = tempfile::Builder::new()
        .prefix("question-")
        .tempdir_in("target")?;
    let operator = Identity::generate(Role::Human, "deniz", "Deniz")?;
    let agent = Identity::generate(Role::Agent, "poyraz", "Poyraz")?;
    let host = transport::start(&temp.path().join("host"), "127.0.0.1:0".parse()?, &operator)?;
    let mut connection = Connection::new(host.address.clone())?;
    let (_, ticket) = connection.offer(&operator, agent.card.clone(), None)?;
    let mut guest = Connection::new(host.address.clone())?;
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
    let profiles = temp.path().join("profiles");
    let codex_home = profiles.join(agent.id()).join("codex-tasks");
    fs::create_dir(&workspace)?;
    let skill_sources = temp.path().join("shared-skills");
    let skill_directory = skill_sources.join("chatoid-fixture");
    fs::create_dir_all(&skill_directory)?;
    fs::write(
        skill_directory.join("SKILL.md"),
        "---\nname: chatoid-fixture\ndescription: A dummy skill for an integration test.\n---\nRetain marker CHATOID-SELECTED-SKILL-813 in this test context.\n",
    )?;
    fs::create_dir_all(&codex_home)?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    fs::write(
        codex_home.join("config.toml"),
        format!(
            r#"model = "gpt-5"
model_provider = "fixture"
[model_providers.fixture]
name = "ChatOid test provider"
base_url = "http://{address}/v1"
wire_api = "responses"
requires_openai_auth = false
"#
        ),
    )?;
    let mut mcp_setup = None;
    if mcp_mode {
        let python = std::process::Command::new("python")
            .args(["-c", "import sys;print(sys.executable)"])
            .output()?;
        ensure!(python.status.success(), "Python fixture unavailable");
        let executable = String::from_utf8(python.stdout)?.trim().to_owned();
        let fixture = temp.path().canonicalize()?.join("mcp.py");
        fs::write(
            &fixture,
            r#"import json, sys, os, time
calls = 0
for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request.get("method")
    if method == "initialize":
        result = {"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}
    elif method == "tools/list":
        result = {"tools":[{"name":"fixture_echo","description":"Echo fixture","inputSchema":{"type":"object","properties":{}}}]}
    elif method == "tools/call":
        calls += 1
        with open(__file__ + ".pid", "w") as pid:
            pid.write(str(os.getpid()))
        time.sleep(MCP_PAUSE)
        result = {"content":[{"type":"text","text":f"MCP-CHAT-VERIFIED-509:{calls}"}, {"type":"image","mimeType":"image/png","data":"MCP_IMAGE_DATA"}],"isError":False}
    else:
        result = {}
    print(json.dumps({"jsonrpc":"2.0","id":request["id"],"result":result}), flush=True)
"#,
        )?;
        use base64::Engine;
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 2).write_to(&mut png, image::ImageFormat::Png)?;
        let image = base64::engine::general_purpose::STANDARD.encode(png.into_inner());
        fs::write(
            &fixture,
            fs::read_to_string(&fixture)?
                .replace("MCP_IMAGE_DATA", &image)
                .replace("MCP_PAUSE", if mcp_cancel { "60" } else { "0" }),
        )?;
        mcp_setup = Some(agent::app_server::McpSetup {
            name: "fixture".into(),
            transport: agent::app_server::McpTransport::Stdio {
                command: executable,
                args: vec![fixture.to_string_lossy().into_owned()],
            },
        });
    }
    listener.set_nonblocking(true)?;
    let (recovery_started, recovery_ready) = mpsc::channel();
    let provider = thread::spawn(move || -> Result<()> {
        for turn in 0..(if mcp_cancel {
            2
        } else if plan_only || fork_mode || mcp_mode {
            5
        } else if recovery_mode {
            4
        } else {
            2
        }) {
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(15))
                    }
                    Err(e) => return Err(e.into()),
                }
            };
            stream.set_nonblocking(false)?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            let mut reader = BufReader::new(&stream);
            let mut request_line = String::new();
            reader.read_line(&mut request_line)?;
            let mut length = None;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
                    break;
                }
                if let Some((header, value)) = line.split_once(':')
                    && header.eq_ignore_ascii_case("content-length")
                {
                    length = Some(value.trim().parse::<usize>()?);
                }
            }
            let length = length.context("Missing provider request length")?;
            ensure!(length < 2 * 1024 * 1024, "Unexpected provider request size");
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes)?;
            let request: Value = serde_json::from_slice(&bytes)?;
            if turn == 0 {
                ensure!(
                    request["tools"]
                        .to_string()
                        .contains("\"name\":\"spawn_agent\"")
                        == delegate,
                    "Session did not propagate the delegation decision"
                );
            }
            if recovery_mode && turn == 2 {
                ensure!(
                    request["input"]
                        .to_string()
                        .contains("RECOVERY-ORIGINAL-762"),
                    "Original recovery task missing"
                );
                recovery_started.send(())?;
                match reader.read(&mut [0; 1]) {
                    Ok(0) => (),
                    Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => (),
                    other => anyhow::bail!("Interrupted request remained open: {other:?}"),
                }
                continue;
            }
            if turn == 3 && request_line.contains("/responses/compact ") {
                let output = json!({
                    "id":"compact_fixture","object":"response.compaction","created_at":0,
                    "output":[{"type":"compaction","id":"cmp_fixture","encrypted_content":"CHATOID-COMPACTED-CONTEXT"}],
                    "usage":{"input_tokens":42,"output_tokens":9,"total_tokens":51,
                        "input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}
                }).to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{output}",
                    output.len()
                )?;
                continue;
            }
            let tool = if plan_only {
                "request_user_input"
            } else {
                "chatoid_ask_user"
            };
            let item = if turn == 0 {
                ensure!(
                    request["input"]
                        .to_string()
                        .contains("CHATOID-SELECTED-SKILL-813"),
                    "The locally selected skill did not reach Codex"
                );
                ensure!(
                    request["tools"].to_string().contains(tool),
                    "Question tool {tool} missing"
                );
                let question = "Hangi çalışma seçeneğini kullanayım?";
                let arguments = if plan_only {
                    json!({"questions":[{"id":"choice","header":"Seçenek","question":question,"options":[{"label":"A","description":"İlk seçenek"},{"label":"B","description":"İkinci seçenek"}]}]})
                } else {
                    json!({"question":question})
                };
                json!({"id":"fc_question", "call_id":"call_question", "type":"function_call",
                    "name":tool, "status":"completed", "arguments":arguments.to_string()})
            } else if recovery_mode && turn == 3 {
                let history = request["input"].to_string();
                ensure!(
                    history.contains("RECOVERY-ORIGINAL-762")
                        && history.contains("The previous attempt was interrupted"),
                    "Recovery did not include the previous task and continuation instructions"
                );
                json!({"id":"msg_recovered", "type":"message", "role":"assistant", "status":"completed", "content":[{"type":"output_text","text":"RECOVERY-COMPLETE-762","annotations":[]}]})
            } else if mcp_mode && (1..=3).contains(&turn) {
                let (name, namespace) = native_fixture_tool(&request["tools"], None)
                    .with_context(|| format!("Native MCP tool missing: {}", request["tools"]))?;
                if turn == 2 {
                    ensure!(
                        !request["input"]
                            .to_string()
                            .contains("MCP-CHAT-VERIFIED-509:1"),
                        "Denied MCP call executed"
                    );
                }
                if turn == 3 {
                    ensure!(
                        request["input"]
                            .to_string()
                            .contains("MCP-CHAT-VERIFIED-509:1"),
                        "First native MCP result missing"
                    );
                    ensure!(
                        request["input"]
                            .to_string()
                            .contains("\"type\":\"input_image\""),
                        "Native MCP image did not reach the model"
                    );
                }
                let mut call = json!({"id":format!("fc_mcp_{turn}"),"call_id":format!("call_mcp_{turn}"),"type":"function_call","name":name,"status":"completed","arguments":"{}"});
                if let Some(namespace) = namespace {
                    call["namespace"] = json!(namespace);
                }
                call
            } else if fork_mode && turn >= 2 {
                let input = request["input"].to_string();
                let marker = match turn {
                    2 => "PARENT-LATE-MARKER",
                    3 => "FORK-ONLY-MARKER",
                    _ => "PARENT-AGAIN-MARKER",
                };
                ensure!(
                    input.contains(marker) && input.contains("ANS-QUOTED-172"),
                    "Expected history missing from Git-free fork fixture"
                );
                if turn >= 3 {
                    ensure!(
                        input.contains("PARENT-LATE-MARKER") == (turn == 4),
                        "Fork did not honor the quoted turn"
                    );
                }
                if turn == 4 {
                    ensure!(
                        !input.contains("FORK-ONLY-MARKER"),
                        "Fork history leaked to parent"
                    );
                }
                json!({"id":format!("msg_fork_{turn}"), "type":"message", "role":"assistant", "status":"completed",
                    "content":[{"type":"output_text","text":marker,"annotations":[]}]})
            } else if turn == 1 && plan_only {
                let input = request["input"].to_string();
                ensure!(
                    input.contains("ANS-QUOTED-172") && !input.contains("WRONG-UNQUOTED-019"),
                    "Wrong answer reached native planning tool"
                );
                json!({"id":"fc_blocked","call_id":"call_blocked","type":"function_call",
                    "name":"chatoid_share_file","status":"completed","arguments":"{\"path\":\"report.txt\"}"})
            } else if turn == 3 && !mcp_mode {
                ensure!(
                    request["input"].to_string().contains("ANS-QUOTED-172"),
                    "Earlier answers were missing from compaction"
                );
                json!({"id":"msg_compacted", "type":"message", "role":"assistant", "status":"completed",
                    "content":[{"type":"output_text","text":"CHATOID-COMPACTED-CONTEXT","annotations":[]}]})
            } else if turn == 4 && !mcp_mode {
                let input = request["input"].to_string();
                ensure!(
                    input.contains("PLAN-RUN-VERIFIED")
                        && input.contains("CHATOID-COMPACTED-CONTEXT"),
                    "Continuation was missing its compacted history"
                );
                json!({"id":"msg_continued", "type":"message", "role":"assistant", "status":"completed",
                    "content":[{"type":"output_text","text":"PLAN-RUN-VERIFIED","annotations":[]}]})
            } else {
                let input = request["input"].to_string();
                ensure!(
                    input.contains("ANS-QUOTED-172") && !input.contains("WRONG-UNQUOTED-019"),
                    "Wrong answer reached the provider"
                );
                if plan_only {
                    ensure!(
                        input.contains("salt okunur planlama görevinde çalıştırılamaz"),
                        "Plan did not reject the file tool"
                    );
                }
                let answer = if plan_only {
                    "<proposed_plan>ANS-QUOTED-172 ile rapor hazırlanacak.</proposed_plan>"
                } else if mcp_mode {
                    ensure!(
                        input.contains("MCP-CHAT-VERIFIED-509:2"),
                        "MCP process state was lost between native calls"
                    );
                    "ANS-QUOTED-172 ve MCP-CHAT-VERIFIED-509 alındı."
                } else {
                    "ANS-QUOTED-172 alındı."
                };
                json!({"id":"msg_answer", "type":"message", "role":"assistant", "status":"completed",
                    "content":[{"type":"output_text","text":answer,"annotations":[]}]})
            };
            let id = format!("resp_question_{turn}");
            let mut body = String::new();
            for (kind, mut event) in [
                (
                    "response.created",
                    json!({"response":{"id":id,"status":"in_progress","output":[]}}),
                ),
                (
                    "response.output_item.done",
                    json!({"output_index":0,"item":item}),
                ),
                (
                    "response.completed",
                    json!({"response":{"id":id,"status":"completed","output":[item],
                        "usage":{"input_tokens":120,"output_tokens":30,"total_tokens":150,
                            "input_tokens_details":{"cached_tokens":40},
                            "output_tokens_details":{"reasoning_tokens":10}}}}),
                ),
            ] {
                event["type"] = json!(kind);
                body.push_str(&format!("event: {kind}\ndata: {event}\n\n"));
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )?;
        }
        Ok(())
    });

    let requester = Session::new(profiles.clone(), operator.clone(), None)?;
    requester
        .send
        .send(Command::Import(pack("host", &host.address)?))?;
    let mut session = Session::new(profiles.clone(), agent.clone(), None)?;
    session.send.send(Command::Settings(Settings {
        manual_task_approval: true,
        workspace: workspace.to_string_lossy().into_owned(),
        workspace_jobs: true,
        skill_roots: vec![skill_sources.clone()],
        ..Settings::default()
    }))?;
    session
        .send
        .send(Command::Pair(pack("card", &operator.card)?))?;
    session
        .send
        .send(Command::Import(pack("host", &host.address)?))?;
    session.send.send(Command::LoadTaskSkills)?;
    let mut send = |text: &str, reply| -> Result<Message> {
        connection.call(
            &operator,
            Action::Send {
                draft: Draft {
                    mode: None,
                    id: Uuid::new_v4(),
                    room,
                    text: text.into(),
                    reply,
                    invitation: None,
                    attachment: None,
                },
            },
        )
    };
    let source = send("Önce bir seçenek sor ve cevabı bekle.", None)?;
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut approved = false;
    let mut answered = false;
    let mut mcp_requested = false;
    let mut mcp_saved = false;
    let mut mcp_approved = 0;
    let mut mcp_stop_sent = false;
    let (initial_reply, initial_thread) = loop {
        let view = match session
            .receive
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(view) => view,
            Err(error) if provider.is_finished() => {
                provider.join().expect("provider fixture")?;
                return Err(error.into());
            }
            Err(error) => return Err(error.into()),
        };
        if mcp_mode && let Some(notice) = &view.notice {
            if notice.starts_with("fixture kaydedildi.") {
                mcp_saved = true;
            }
            ensure!(
                !notice.starts_with("MCP bağlantısı kaydedilemedi"),
                "{notice}"
            );
        }
        let Some(job) = view.local.jobs.iter().find(|job| job.source == source.seq) else {
            continue;
        };
        if mcp_cancel
            && !mcp_stop_sent
            && job.status == WorkspaceJobStatus::Running
            && temp.path().join("mcp.py.pid").exists()
        {
            session.send.send(Command::CancelWorkspaceJob(job.id))?;
            mcp_stop_sent = true;
        }
        if mcp_cancel && job.status == WorkspaceJobStatus::Cancelled {
            assert!(mcp_stop_sent && mcp_approved == 1);
            assert!(
                !view.local.hosts[&host.address.pin]
                    .messages
                    .values()
                    .any(|message| message.client_id == job.id)
            );
            provider.join().expect("cancel provider")?;
            #[cfg(windows)]
            assert_fixture_process_stopped(&temp.path().join("mcp.py.pid"))?;
            return Ok(());
        }
        if !approved && job.status == WorkspaceJobStatus::Pending {
            let Some(skills) = view.local.task_skills.as_ref() else {
                if let Some(error) = view
                    .notice
                    .as_deref()
                    .filter(|note| note.contains("becerileri yüklenemedi"))
                {
                    anyhow::bail!("{error}");
                }
                continue;
            };
            let skill = skills
                .iter()
                .find(|skill| skill.name == "chatoid-fixture")
                .context("Project skill not listed")?;
            if mcp_mode && view.local.task_mcp.is_none() {
                if let Some(setup) = mcp_setup.take() {
                    session.send.send(Command::AddTaskMcp(setup))?;
                } else if mcp_saved && !mcp_requested {
                    session.send.send(Command::LoadTaskMcp)?;
                    mcp_requested = true;
                }
                continue;
            }
            session.send.send(Command::DecideWorkspaceJob(
                job.id,
                WorkspaceDecision::Approve {
                    web_research: false,
                    network: false,
                    delegate,
                    model: None,
                    plan_only,
                    skills: vec![skill.path.clone()],
                    compact_before: false,
                    fork: false,
                    mcp: mcp_mode,
                },
            ))?;
            approved = true;
        }
        if let WorkspaceJobStatus::Failed(error) = &job.status {
            anyhow::bail!("{error}");
        }
        if let Some(approval) = &view.local.task_approval {
            ensure!(mcp_mode && mcp_approved < 3, "Unexpected approval in task");
            assert_eq!(approval.title, "fixture / fixture_echo");
            assert!(approval.action.starts_with("MCP araç çağrısı"));
            assert!(approval.action.contains("{}"));
            session.send.send(Command::DecideTaskAction(
                approval.id,
                mcp_approved != 0 || mcp_cancel,
            ))?;
            mcp_approved += 1;
        }
        if plan_only {
            assert!(
                view.local.task_approval.is_none(),
                "A read-only task should not ask to share an output"
            );
        }
        let messages = &view.local.hosts[&host.address.pin].messages;
        if !answered
            && let Some(question) = messages
                .values()
                .find(|m| m.text.starts_with(TASK_QUESTION))
        {
            assert!(question.text.contains("Hangi çalışma seçeneğini"));
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let update = requester
                    .receive
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
                if update
                    .local
                    .hosts
                    .get(&host.address.pin)
                    .is_some_and(|link| {
                        link.task_activity.iter().any(|activity| {
                            activity.source == source.seq
                                && activity.phase == TaskPhase::WaitingAnswer
                        })
                    })
                {
                    break;
                }
            }
            send("WRONG-UNQUOTED-019", None)?;
            send("ANS-QUOTED-172", Some(question.seq))?;
            answered = true;
        }
        if let Some(result) = messages.values().find(|m| m.client_id == job.id) {
            assert_eq!(job.delegate, delegate);
            assert!(approved && answered);
            assert!(result.text.contains("ANS-QUOTED-172"));
            if mcp_mode {
                assert!(mcp_saved);
                assert!(mcp_approved == 3 && result.text.contains("MCP-CHAT-VERIFIED-509"));
            }
            assert_eq!(job.skill_names, ["chatoid-fixture"]);
            let usage = view
                .local
                .workspace_reports
                .get(&job.id)
                .and_then(|report| report.usage.as_ref())
                .context("Codex usage was not retained")?;
            assert_eq!(usage.last.input_tokens, 120);
            assert_eq!(usage.last.cached_input_tokens, 40);
            assert_eq!(usage.last.output_tokens, 30);
            assert_eq!(usage.last.reasoning_output_tokens, 10);
            assert!(usage.total.total_tokens >= 150);
            assert_eq!(
                result.reply.as_ref().context("original task quote")?.id,
                source.seq
            );
            if plan_only {
                assert!(!result.text.contains("<proposed_plan>"));
            }
            break (
                result.clone(),
                view.local
                    .workspace_threads
                    .get(&job.id)
                    .context("Initial continuation missing")?
                    .clone(),
            );
        }
    };
    #[cfg(windows)]
    if mcp_mode {
        assert_fixture_process_stopped(&temp.path().join("mcp.py.pid"))?;
    }
    if recovery_mode {
        let interrupted = send("RECOVERY-ORIGINAL-762", None)?;
        let approve = |session: &Session, id| -> Result<()> {
            session.send.send(Command::DecideWorkspaceJob(
                id,
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
            Ok(())
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut approved = false;
        let mut ready = false;
        let (job_id, saved_thread) = loop {
            let view = session
                .receive
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .context("Waiting for recovery checkpoint")?;
            let Some(job) = view
                .local
                .jobs
                .iter()
                .find(|job| job.source == interrupted.seq)
            else {
                continue;
            };
            if !approved && job.status == WorkspaceJobStatus::Pending {
                approve(&session, job.id)?;
                approved = true;
            }
            ready |= recovery_ready.try_recv().is_ok();
            if ready && job.recovery.is_some() && job.status == WorkspaceJobStatus::Running {
                assert!(!view.local.workspace_threads.contains_key(&job.id));
                assert!(
                    !view.local.hosts[&host.address.pin]
                        .messages
                        .values()
                        .any(|message| message.client_id == job.id)
                );
                break (
                    job.id,
                    job.recovery
                        .as_ref()
                        .context("Checkpoint missing during execution")?
                        .thread_id
                        .clone(),
                );
            }
            if let WorkspaceJobStatus::Failed(error) = &job.status {
                anyhow::bail!("{error}")
            }
        };
        drop(session);
        let deadline = Instant::now() + Duration::from_secs(10);
        session = loop {
            match Session::new(profiles.clone(), agent.clone(), None) {
                Ok(session) => break session,
                Err(error) if Instant::now() < deadline && error.to_string().contains("açık") => {
                    thread::sleep(Duration::from_millis(50))
                }
                Err(error) => return Err(error),
            }
        };
        let restored = session.receive.recv_timeout(Duration::from_secs(10))?;
        let job = restored
            .local
            .jobs
            .iter()
            .find(|job| job.id == job_id)
            .context("Interrupted job lost on restart")?;
        assert!(
            matches!(&job.status, WorkspaceJobStatus::Failed(error) if error.contains("kapanırken"))
        );
        assert_eq!(job.recovery.as_ref().unwrap().thread_id, saved_thread);
        if !restored.local.hosts[&host.address.pin].online {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let view = session
                    .receive
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .context("Waiting for restored host connection")?;
                if view.local.hosts[&host.address.pin].online {
                    break;
                }
            }
        }
        session.send.send(Command::RetryWorkspaceJob(job_id))?;
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut reapproved = false;
        loop {
            let view = session
                .receive
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .context("Waiting for resumed task result")?;
            let job = view
                .local
                .jobs
                .iter()
                .find(|job| job.id == job_id)
                .context("Recovered job missing")?;
            if !reapproved && job.status == WorkspaceJobStatus::Pending {
                assert_eq!(job.approval_round, 1);
                assert!(!job.network && !job.web_research && !job.plan_only && job.model.is_none());
                assert!(!job.delegate);
                assert_eq!(job.continuation.as_ref().unwrap().thread_id, saved_thread);
                assert!(!job.can_fork());
                assert!(view.local.task_approval.is_none());
                approve(&session, job_id)?;
                reapproved = true;
            }
            if let WorkspaceJobStatus::Failed(error) = &job.status {
                anyhow::bail!("{error}")
            }
            let replies = view.local.hosts[&host.address.pin]
                .messages
                .values()
                .filter(|message| message.client_id == job_id)
                .collect::<Vec<_>>();
            if let Some(reply) = replies.first() {
                assert_eq!(replies.len(), 1);
                assert_eq!(reply.text, "RECOVERY-COMPLETE-762");
                assert_eq!(
                    view.local.workspace_threads[&job_id].thread_id,
                    saved_thread
                );
                assert!(job.recovery.is_none());
                break;
            }
        }
    }
    if fork_mode {
        let mut continue_from = |source: &Message,
                                 marker: &str,
                                 fork: bool|
         -> Result<(Message, WorkspaceContinuation)> {
            let request = send(&format!("/devam {marker}"), Some(source.seq))?;
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut approved = false;
            loop {
                let view = session
                    .receive
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
                let Some(job) = view.local.jobs.iter().find(|job| job.source == request.seq) else {
                    continue;
                };
                if !approved && job.status == WorkspaceJobStatus::Pending {
                    assert!(job.can_fork());
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
                            fork,
                            mcp: false,
                        },
                    ))?;
                    approved = true;
                }
                if let WorkspaceJobStatus::Failed(error) = &job.status {
                    anyhow::bail!("{error}");
                }
                if let Some(reply) = view.local.hosts[&host.address.pin]
                    .messages
                    .values()
                    .find(|message| message.client_id == job.id)
                {
                    assert_eq!(reply.text, marker);
                    assert_eq!(job.forked, fork);
                    let continued = view
                        .local
                        .workspace_threads
                        .get(&job.id)
                        .context("Continuation missing")?;
                    return Ok((reply.clone(), continued.clone()));
                }
            }
        };
        let (late, late_thread) = continue_from(&initial_reply, "PARENT-LATE-MARKER", false)?;
        assert_eq!(late_thread.thread_id, initial_thread.thread_id);
        assert_ne!(late_thread.turn_id, initial_thread.turn_id);
        let (_, branch) = continue_from(&initial_reply, "FORK-ONLY-MARKER", true)?;
        assert_ne!(branch.thread_id, initial_thread.thread_id);
        let (_, parent) = continue_from(&late, "PARENT-AGAIN-MARKER", false)?;
        assert_eq!(parent.thread_id, initial_thread.thread_id);
    }
    if plan_only {
        let continuation = send(
            "/devam PLAN-RUN-VERIFIED ile uygula.",
            Some(initial_reply.seq),
        )?;
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut approved = false;
        loop {
            let view = session
                .receive
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
            let Some(job) = view
                .local
                .jobs
                .iter()
                .find(|job| job.source == continuation.seq)
            else {
                continue;
            };
            assert!(job.continuation.is_some());
            if !approved && job.status == WorkspaceJobStatus::Pending {
                session.send.send(Command::DecideWorkspaceJob(
                    job.id,
                    WorkspaceDecision::Approve {
                        web_research: false,
                        network: false,
                        delegate: false,
                        model: None,
                        plan_only: false,
                        skills: Vec::new(),
                        compact_before: true,
                        fork: false,
                        mcp: false,
                    },
                ))?;
                approved = true;
            }
            if let WorkspaceJobStatus::Failed(error) = &job.status {
                anyhow::bail!("{error}");
            }
            if let Some(reply) = view.local.hosts[&host.address.pin]
                .messages
                .values()
                .find(|message| message.client_id == job.id)
            {
                assert_eq!(reply.text, "PLAN-RUN-VERIFIED");
                assert_eq!(
                    reply.reply.as_ref().context("Continuation reply")?.id,
                    continuation.seq
                );
                assert!(approved && !job.plan_only);
                assert!(
                    view.local
                        .workspace_reports
                        .get(&job.id)
                        .is_some_and(|report| report.context_compacted)
                );
                break;
            }
        }
    }
    provider.join().expect("provider fixture")?;
    drop(session);
    let deadline = Instant::now() + Duration::from_secs(10);
    let restarted = loop {
        match Session::new(profiles.clone(), agent.clone(), None) {
            Ok(session) => break session,
            Err(error) if Instant::now() < deadline && error.to_string().contains("açık") => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error),
        }
    };
    let restored = restarted.receive.recv_timeout(Duration::from_secs(10))?;
    assert_eq!(restored.local.settings.skill_roots, vec![skill_sources]);
    let report = restored
        .local
        .workspace_reports
        .get(&initial_reply.client_id)
        .context("Completed report was lost after restarting the agent")?;
    assert_eq!(
        report
            .usage
            .as_ref()
            .context("Usage missing after restart")?
            .last
            .input_tokens,
        120
    );
    let mut legacy = serde_json::to_value(&restored.local)?;
    legacy["settings"]
        .as_object_mut()
        .context("Expected legacy settings")?
        .remove("skill_roots");
    legacy
        .as_object_mut()
        .context("Expected local object")?
        .remove("workspace_reports");
    for job in legacy["jobs"]
        .as_array_mut()
        .context("Expected legacy jobs")?
    {
        let job = job.as_object_mut().context("Expected legacy job")?;
        job.remove("recovery");
        job.remove("approval_round");
        job.remove("delegate");
    }
    let old: Local = serde_json::from_value(legacy)?;
    assert!(old.workspace_reports.is_empty());
    assert!(old.settings.skill_roots.is_empty());
    assert!(
        old.jobs
            .iter()
            .all(|job| job.recovery.is_none() && job.approval_round == 0)
    );
    Ok(())
}
