use anyhow::{Context, Result, ensure};
use chatoid::agent::{
    Settings,
    app_server::{TaskInput, TaskThread, run_task},
};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[test]
#[ignore = "requires local Codex App Server; uses a simulated provider on loopback"]
fn native_subagent_reports_its_result_to_the_parent() -> Result<()> {
    exercise_native_subagent(false)
}

#[test]
#[ignore = "requires local Codex App Server; uses a simulated provider on loopback"]
fn cancelling_the_parent_closes_the_subagent_provider_request() -> Result<()> {
    exercise_native_subagent(true)
}

fn exercise_native_subagent(cancel_child: bool) -> Result<()> {
    let root = tempfile::Builder::new()
        .prefix("delegation-")
        .tempdir_in("target")?;
    let home = root.path().join("codex");
    let workspace = root.path().join("work");
    std::fs::create_dir(&home)?;
    std::fs::create_dir(&workspace)?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    std::fs::write(
        home.join("config.toml"),
        format!(
            r#"model = "gpt-5"
model_provider = "fixture"
[model_providers.fixture]
name = "ChatOid delegation fixture"
base_url = "http://{address}/v1"
wire_api = "responses"
requires_openai_auth = false
"#
        ),
    )?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let child_observed = Arc::new(AtomicBool::new(false));
    let provider_cancel = cancelled.clone();
    let provider_observed = child_observed.clone();
    let provider = thread::spawn(move || -> Result<()> {
        let mut parent_step = 0;
        let mut child_finished = false;
        for serial in 0..4 {
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            stream.set_nonblocking(false)?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            let mut reader = BufReader::new(&stream);
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
            let length = length.context("Provider request length missing")?;
            ensure!(length < 2 * 1024 * 1024, "Unexpected provider request size");
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes)?;
            let request: Value = serde_json::from_slice(&bytes)?;
            let history = request["input"].to_string();
            let message = |text: &str| json!({"id":format!("msg_{serial}"),"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]});
            let call = |name: &str, args: Value| json!({"id":format!("fc_{serial}"),"call_id":format!("call_{serial}"),"type":"function_call","namespace":"collaboration","name":name,"arguments":args.to_string(),"encrypted_function_args":[],"status":"completed"});
            let item = if !history.contains("DELEGATION-ROOT-625")
                && history.contains("SUBAGENT-REQUEST-939")
            {
                ensure!(
                    !child_finished,
                    "Child unexpectedly called the provider twice"
                );
                if cancel_child {
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !provider_observed.load(Ordering::Acquire) && Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(10));
                    }
                    ensure!(
                        provider_observed.load(Ordering::Acquire),
                        "Native child was not reported before cancellation"
                    );
                    provider_cancel.store(true, Ordering::Release);
                    match reader.read(&mut [0; 1]) {
                        Ok(0) => (),
                        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => (),
                        other => anyhow::bail!(
                            "Subagent request did not close after cancellation: {other:?}"
                        ),
                    }
                    return Ok(());
                }
                child_finished = true;
                message("CHILD-RESULT-287")
            } else {
                ensure!(
                    history.contains("DELEGATION-ROOT-625"),
                    "Unknown agent requested a response"
                );
                let item = match parent_step {
                    0 => {
                        ensure!(
                            request["tools"]
                                .to_string()
                                .contains("\"name\":\"spawn_agent\""),
                            "Native spawn tool missing"
                        );
                        call(
                            "spawn_agent",
                            json!({"task_name":"review","message":"SUBAGENT-REQUEST-939 Return CHILD-RESULT-287.","fork_turns":"none"}),
                        )
                    }
                    1 => {
                        ensure!(
                            history.contains("task_name"),
                            "Spawn did not return a task name"
                        );
                        call("wait_agent", json!({"timeout_ms":10000}))
                    }
                    _ => {
                        ensure!(
                            child_finished && history.contains("CHILD-RESULT-287"),
                            "Child result did not reach the parent"
                        );
                        message("ROOT-DONE CHILD-RESULT-287")
                    }
                };
                parent_step += 1;
                item
            };
            let id = format!("resp_{serial}");
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
                    json!({"response":{"id":id,"status":"completed","output":[item]}}),
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
        ensure!(
            parent_step == 3 && child_finished,
            "Delegation fixture did not finish"
        );
        Ok(())
    });
    let latest = Mutex::new(None);
    let result = run_task(
        &Settings {
            task_timeout_minutes: 1,
            ..Settings::default()
        },
        &workspace,
        TaskInput {
            text: "DELEGATION-ROOT-625 Delegate one independent review subtask and wait for its result.",
            attachment: None,
            web_research: false,
            network: false,
            delegate: true,
            steering: None,
            model: None,
            plan_only: false,
            review: None,
            skills: &[],
            compact_before: false,
        },
        TaskThread::New(&home),
        &cancelled,
        None,
        |progress| {
            if !progress.agents.is_empty() {
                child_observed.store(true, Ordering::Release);
            }
            *latest.lock().unwrap() = Some(progress);
        },
    );
    provider.join().expect("delegation provider")?;
    if cancel_child {
        assert!(
            result
                .err()
                .context("Interrupted task returned a result")?
                .to_string()
                .contains("iptal edildi")
        );
        return Ok(());
    }
    assert_eq!(result?.text, "ROOT-DONE CHILD-RESULT-287");
    let report = latest.into_inner().unwrap().context("Progress missing")?;
    assert_eq!(report.agents.len(), 1);
    let child = &report.agents[0];
    assert_eq!(child.name, "/root/review");
    assert_eq!(child.model.as_deref(), Some("gpt-5"));
    assert_eq!(child.state, "completed");
    let restored: chatoid::agent::app_server::TaskProgress =
        serde_json::from_slice(&serde_json::to_vec(&report)?)?;
    assert_eq!(restored.agents, report.agents);
    Ok(())
}
