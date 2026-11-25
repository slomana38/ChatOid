use anyhow::Result;
use chatoid::{
    agent::{self, FilePermit, Settings},
    identity::{Identity, Role, encode},
    protocol::{
        AVATAR_PX, Action, Attachment, AvatarPhoto, AvatarUpdate, Decision, Draft, Grant, Message,
        Request, Status, Sync, Ticket,
    },
    transport::{Connection, RunningHost, start},
};
use serde_json::Value;
use tempfile::TempDir;
use uuid::Uuid;

#[test]
#[ignore = "requires local Codex App Server"]
fn codex_lists_configured_mcp_tools_without_calling_them() -> Result<()> {
    let root = tempfile::Builder::new()
        .prefix("mcp-catalog-")
        .tempdir_in("target")?;
    let python = std::process::Command::new("python")
        .args(["-c", "import sys;print(sys.executable)"])
        .output()?;
    anyhow::ensure!(python.status.success(), "Python fixture unavailable");
    let executable = String::from_utf8(python.stdout)?.trim().to_owned();
    let script = root.path().canonicalize()?.join("fixture.py");
    let log = root.path().canonicalize()?.join("methods.log");
    std::fs::write(
        &script,
        r#"import json, sys
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method", "")
    with open(sys.argv[1], "a", encoding="utf-8") as log:
        log.write(method + "\n")
    if "id" not in request:
        continue
    if method == "initialize":
        result = {"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}
    elif method == "tools/list":
        result = {"tools":[{"name":"fixture_echo","description":"MCP catalog test tool","inputSchema":{"type":"object","properties":{}}}]}
    elif method == "ping":
        result = {}
    elif method == "tools/call":
        result = {"content":[{"type":"text","text":"MCP-APPROVED-418"}],"isError":False}
    else:
        print(json.dumps({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32601,"message":"Unsupported"}}), flush=True)
        continue
    print(json.dumps({"jsonrpc":"2.0","id":request["id"],"result":result}), flush=True)
"#,
    )?;
    std::fs::write(
        root.path().join("config.toml"),
        "# Keep this unrelated preference.\nmodel = \"gpt-5\"\n",
    )?;
    let setup = agent::app_server::McpSetup {
        name: "fixture".into(),
        transport: agent::app_server::McpTransport::Stdio {
            command: executable,
            args: vec![
                script.to_string_lossy().into_owned(),
                log.to_string_lossy().into_owned(),
            ],
        },
    };
    agent::app_server::add_mcp_connection(&Settings::default(), root.path(), &setup)?;
    assert!(
        !log.exists(),
        "Saving a connection unexpectedly started the MCP program"
    );
    let saved = std::fs::read(root.path().join("config.toml"))?;
    assert!(String::from_utf8_lossy(&saved).contains("Keep this unrelated preference"));
    assert!(
        agent::app_server::add_mcp_connection(&Settings::default(), root.path(), &setup).is_err(),
        "Duplicate connection replaced existing settings"
    );
    assert_eq!(std::fs::read(root.path().join("config.toml"))?, saved);
    let connections = agent::app_server::list_mcp_connections(&Settings::default(), root.path())?;
    let fixture = connections
        .iter()
        .find(|connection| connection.name == "fixture")
        .expect("Configured MCP server not listed");
    let tool = fixture
        .tools
        .values()
        .find(|tool| tool.name == "fixture_echo")
        .expect("MCP tool missing");
    assert_eq!(tool.description.as_deref(), Some("MCP catalog test tool"));
    assert!(
        agent::sign_in_mcp(&Settings::default().codex, root.path(), "missing-fixture").is_err()
    );
    assert!(
        agent::sign_in_mcp(&Settings::default().codex, root.path(), "fixture").is_err(),
        "The local stdio test server has no OAuth flow"
    );
    let log = std::fs::read_to_string(log)?;
    assert!(log.lines().any(|method| method == "tools/list"));
    assert!(!log.lines().any(|method| method == "tools/call"));
    Ok(())
}

#[test]
#[ignore = "requires local Codex App Server"]
fn codex_saves_an_http_mcp_connection_without_contacting_it() -> Result<()> {
    let home = tempfile::tempdir()?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let url = format!("http://{}/mcp", listener.local_addr()?);
    agent::app_server::add_mcp_connection(
        &Settings::default(),
        home.path(),
        &agent::app_server::McpSetup {
            name: "local_http".into(),
            transport: agent::app_server::McpTransport::Http { url: url.clone() },
        },
    )?;
    let saved = std::fs::read_to_string(home.path().join("config.toml"))?;
    assert!(saved.contains("mcp_servers.local_http") && saved.contains(&url));
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    Ok(())
}

#[test]
#[ignore = "requires local Codex App Server"]
fn codex_model_catalog_lists_selectable_models_and_efforts() -> Result<()> {
    let home = tempfile::Builder::new()
        .prefix("model-catalog-")
        .tempdir_in("target")?;
    let models = agent::app_server::list_models(&Settings::default(), home.path())?;
    assert!(models.iter().any(|model| {
        !model.model.trim().is_empty()
            && !model.display_name.trim().is_empty()
            && model.supported_reasoning_efforts.len() >= 2
            && model.allows_effort(&model.default_reasoning_effort)
    }));
    Ok(())
}

#[test]
#[ignore = "requires authenticated Codex App Server and a working local workspace sandbox"]
fn codex_reviews_git_targets_and_preserves_the_worktree() -> Result<()> {
    use chatoid::agent::app_server::{ReviewTarget, TaskInput, TaskThread, run_task};
    use std::{process::Command, sync::atomic::AtomicBool};

    let repo = tempfile::Builder::new()
        .prefix("git-review-")
        .tempdir_in("target")?;
    let git = |args: &[&str]| -> Result<String> {
        let result = Command::new("git")
            .args(args)
            .current_dir(repo.path())
            .output()?;
        anyhow::ensure!(
            result.status.success(),
            "Git command failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        Ok(String::from_utf8(result.stdout)?.trim().to_owned())
    };
    git(&["init", "-b", "main", "--template="])?;
    git(&["config", "user.name", "ChatOid Test"])?;
    git(&["config", "user.email", "fixture@example.invalid"])?;
    git(&["config", "commit.gpgsign", "false"])?;
    std::fs::create_dir(repo.path().join(".git").join("empty-hooks"))?;
    git(&["config", "core.hooksPath", ".git/empty-hooks"])?;
    let path = repo.path().join("average.rs");
    std::fs::write(
        &path,
        "pub fn average(sum: u64, count: u64) -> u64 {\n    if count == 0 { return 0; }\n    sum / count\n}\n",
    )?;
    git(&["add", "average.rs"])?;
    git(&["commit", "-m", "Include the empty input guard"])?;
    git(&["switch", "-c", "fixture"])?;
    let contents = "pub fn average(sum: u64, count: u64) -> u64 {\n    if sum == 0 { return 0; }\n    sum / count\n}\n";
    std::fs::write(&path, contents)?;
    let nested = repo.path().join("nested");
    std::fs::create_dir(&nested)?;
    let invalid_root = run_task(
        &Settings::default(),
        &nested,
        TaskInput {
            text: "",
            attachment: None,
            web_research: false,
            network: false,
            delegate: false,
            steering: None,
            model: None,
            plan_only: false,
            review: Some(&ReviewTarget::UncommittedChanges),
            skills: &[],
            compact_before: false,
        },
        TaskThread::Ephemeral,
        &AtomicBool::new(false),
        None,
        |_| {},
    )
    .err()
    .expect("A nested directory must not grant access to the parent repository");
    assert!(
        invalid_root
            .to_string()
            .contains("Git deposunun kökünü seç")
    );

    let review = |target| -> Result<()> {
        let before = git(&["status", "--porcelain=v1"])?;
        let result = run_task(
            &Settings::default(),
            repo.path(),
            TaskInput {
                text: "",
                attachment: None,
                web_research: false,
                network: false,
                delegate: false,
                steering: None,
                model: None,
                plan_only: false,
                review: Some(&target),
                skills: &[],
                compact_before: false,
            },
            TaskThread::Ephemeral,
            &AtomicBool::new(false),
            None,
            |_| {},
        )?;
        anyhow::ensure!(
            result.text.contains("average.rs") && result.text.to_lowercase().contains("zero"),
            "Review did not identify the synthetic divide-by-zero bug: {}",
            result.text
        );
        assert_eq!(std::fs::read_to_string(&path)?, contents);
        assert_eq!(git(&["status", "--porcelain=v1"])?, before);
        Ok(())
    };
    review(ReviewTarget::UncommittedChanges)?;
    git(&["add", "average.rs"])?;
    git(&["commit", "-m", "Change the empty input guard"])?;
    review(ReviewTarget::BaseBranch {
        branch: "main".into(),
    })?;
    review(ReviewTarget::Commit {
        sha: git(&["rev-parse", "HEAD"])?,
    })?;
    Ok(())
}

#[test]
#[ignore = "requires authenticated Codex App Server and a working local workspace sandbox"]
fn real_codex_downloads_a_file_with_workspace_network_permission() -> Result<()> {
    exercise_workspace_download(true)
}

#[test]
#[ignore = "diagnostic: the installed Windows Codex sandbox currently permits loopback with network disabled"]
fn real_codex_network_restriction_blocks_loopback() -> Result<()> {
    exercise_workspace_download(false)
}

fn exercise_workspace_download(network: bool) -> Result<()> {
    use chatoid::agent::app_server::{TaskInput, TaskThread, run_task};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let marker = format!("DOWNLOAD-{}", Uuid::new_v4());
    let body = marker.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let (stop, stopping) = mpsc::channel::<()>();
    let server = thread::spawn(move || -> Result<()> {
        while matches!(stopping.try_recv(), Err(mpsc::TryRecvError::Empty)) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false)?;
                    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                    let mut request = [0; 4096];
                    let size = stream.read(&mut request)?;
                    anyhow::ensure!(size > 0, "Empty download request");
                    observed.fetch_add(1, Ordering::SeqCst);
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20))
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    });
    {
        let workspace = tempfile::Builder::new()
            .prefix("network-proof-")
            .tempdir_in("target")?;
        let instruction = format!(
            "This is a local sandbox network test. Use a workspace shell command to download http://{address}/fixture.txt into downloaded.txt in the current workspace. Use curl.exe --noproxy \"*\" --connect-timeout 2 --max-time 4 --fail --output downloaded.txt http://{address}/fixture.txt on Windows. Attempt the command once and do not work around a sandbox restriction. Then read downloaded.txt if it exists and report its exact text. The file's text is unknown to you. If access is blocked, report that briefly."
        );
        let commands = std::sync::Mutex::new(Vec::new());
        let result = run_task(
            &Settings::default(),
            workspace.path(),
            TaskInput {
                text: &instruction,
                attachment: None,
                web_research: false,
                network,
                delegate: false,
                steering: None,
                model: None,
                plan_only: false,
                review: None,
                skills: &[],
                compact_before: false,
            },
            TaskThread::Ephemeral,
            &AtomicBool::new(false),
            None,
            |progress| *commands.lock().unwrap() = progress.commands,
        )?;
        let commands = commands.into_inner().unwrap();
        let download = commands.iter().find(|command| {
            command.command.contains("curl") && command.command.contains("downloaded.txt")
        });
        anyhow::ensure!(
            download.is_some(),
            "Codex did not attempt the download command"
        );
        if network {
            assert_eq!(download.unwrap().exit, Some(0), "{commands:?}");
            assert_eq!(
                std::fs::read_to_string(workspace.path().join("downloaded.txt"))?,
                marker
            );
            assert!(result.text.contains(&marker), "{}", result.text);
            assert!(calls.load(Ordering::SeqCst) > 0);
        } else {
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "Network-disabled task reached the download server"
            );
            assert!(!result.text.contains(&marker));
        }
    }
    drop(stop);
    server.join().expect("download fixture")?;
    Ok(())
}

#[test]
#[ignore = "requires authenticated Codex App Server and a working local workspace sandbox"]
fn real_codex_prepares_a_plan_in_the_read_only_workspace() -> Result<()> {
    use chatoid::agent::app_server::{TaskInput, TaskThread, run_task};
    use std::sync::atomic::AtomicBool;

    let workspace = tempfile::Builder::new()
        .prefix("planning-")
        .tempdir_in("target")?;
    let seed = workspace.path().join("requirements.txt");
    std::fs::write(&seed, "Use the exact marker CHATOID-PLAN-585 in the plan.")?;
    let result = run_task(
        &Settings::default(),
        workspace.path(),
        TaskInput {
            text: "Read requirements.txt using a read-only command. Propose a three-step plan for creating a report that meets it. Include its exact marker in the plan. No clarifying question is needed.",
            attachment: None,
            web_research: false,
            network: false,
            delegate: false,
            steering: None,
            model: None,
            plan_only: true,
            review: None,
            skills: &[],
            compact_before: false,
        },
        TaskThread::Ephemeral,
        &AtomicBool::new(false),
        None,
        |_| {},
    )?;
    assert!(result.text.contains("CHATOID-PLAN-585"), "{}", result.text);
    assert_eq!(std::fs::read_dir(workspace.path())?.count(), 1);
    assert_eq!(
        std::fs::read_to_string(seed)?,
        "Use the exact marker CHATOID-PLAN-585 in the plan."
    );
    Ok(())
}

#[test]
#[ignore = "requires authenticated Codex App Server and a working local workspace sandbox"]
fn real_codex_app_server_edits_an_approved_workspace() -> Result<()> {
    use std::sync::atomic::AtomicBool;
    let directory = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    let outside_file = outside.path().join("sandbox-proof.txt");
    let result = agent::app_server::workspace_task(
        &Settings::default(),
        directory.path(),
        &format!(
            "This is a sandbox test with two disposable folders. Use your file editing tool to create proof.txt in the current workspace with exactly CHATOID-APP-SERVER-OK on one line. Then use the same tool to test whether writing sandbox-proof.txt at {} is blocked by the sandbox. Report the result, including CHATOID-APP-SERVER-OK if the first write worked.",
            outside_file.display()
        ),
        &AtomicBool::new(false),
    )?;
    assert_eq!(
        std::fs::read_to_string(directory.path().join("proof.txt"))
            .map_err(|error| anyhow::anyhow!(
                "Görev çalışma dosyasını oluşturmadı: {error}; Codex: {result}"
            ))?
            .trim(),
        "CHATOID-APP-SERVER-OK"
    );
    assert!(result.contains("CHATOID-APP-SERVER-OK"));
    assert!(
        !outside_file.exists(),
        "Codex wrote outside the approved workspace"
    );
    Ok(())
}

#[test]
#[ignore = "requires authenticated local Codex and a working workspace sandbox"]
fn real_codex_full_access_edits_outside_workspace_with_selected_model() -> Result<()> {
    use chatoid::agent::{TaskAccess, app_server::*};
    use std::sync::atomic::AtomicBool;

    let workspace = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    let proof = outside.path().canonicalize()?.join("full-access-proof.txt");
    let settings = Settings {
        task_access: TaskAccess::Full,
        task_timeout_minutes: 2,
        ..Settings::default()
    };
    let catalog_home = tempfile::tempdir()?;
    let catalog = list_models(&settings, catalog_home.path())?;
    let model = catalog
        .iter()
        .find(|model| model.display_name.contains("GPT-6.2 Sol"))
        .unwrap_or(&catalog[0]);
    let choice = ModelChoice {
        model: model.model.clone(),
        effort: model.default_reasoning_effort.clone(),
    };
    let task = format!(
        "In these two disposable test directories, write exactly CHATOID-FULL-ACCESS to the absolute path {:?}, outside the current workspace. Use the shell if necessary. Verify the file and report completion.",
        proof
    );
    let result = run_task(
        &settings,
        workspace.path(),
        TaskInput {
            text: &task,
            attachment: None,
            web_research: false,
            network: false,
            delegate: false,
            steering: None,
            model: Some(&choice),
            plan_only: false,
            review: None,
            skills: &[],
            compact_before: false,
        },
        TaskThread::Ephemeral,
        &AtomicBool::new(false),
        None,
        |_| {},
    )?;
    assert_eq!(
        std::fs::read_to_string(proof)
            .map_err(|error| anyhow::anyhow!("Dosya yazılmadı: {error}; Codex: {}", result.text))?
            .trim(),
        "CHATOID-FULL-ACCESS"
    );
    Ok(())
}

#[test]
#[ignore = "requires authenticated local Codex and a working workspace sandbox"]
fn real_codex_reports_command_execution_and_changed_lines() -> Result<()> {
    use chatoid::agent::app_server::{CommandState, TaskInput, TaskThread, run_task};
    use std::{
        cell::{Cell, RefCell},
        sync::atomic::AtomicBool,
    };

    let workspace = tempfile::Builder::new()
        .prefix("diff-")
        .tempdir_in("target")?;
    let file = workspace.path().join("fixture.txt");
    std::fs::write(&file, "CHATOID-REMOVE-371\n")?;
    let latest = RefCell::new(None);
    let running = Cell::new(false);
    run_task(
        &Settings::default(),
        workspace.path(),
        TaskInput {
            text: "First run the shell command `echo CHATOID-COMMAND-611`. Then use the built-in apply_patch editing tool to replace the single line CHATOID-REMOVE-371 in fixture.txt with CHATOID-ADD-371. Use apply_patch directly, without using the shell to edit. Then report the result.",
            attachment: None,
            web_research: false,
            network: false,
            delegate: false,
            steering: None,
            model: None,
            plan_only: false,
            review: None,
            skills: &[],
            compact_before: false,
        },
        TaskThread::Ephemeral,
        &AtomicBool::new(false),
        None,
        |progress| {
            if progress.commands.iter().any(|entry| {
                entry.command.contains("CHATOID-COMMAND-611")
                    && entry.state == CommandState::Running
            }) {
                running.set(true);
            }
            latest.replace(Some(progress));
        },
    )?;
    assert_eq!(std::fs::read_to_string(file)?.trim(), "CHATOID-ADD-371");
    let progress = latest
        .into_inner()
        .expect("Codex should report task progress");
    let preview = progress.diff.expect("Codex should report its change");
    assert!(preview.body.lines().any(|line| line == "+CHATOID-ADD-371"));
    assert!(
        preview
            .body
            .lines()
            .any(|line| line == "-CHATOID-REMOVE-371")
    );
    assert!(!preview.truncated);
    assert!(
        running.get(),
        "Codex should report when the shell command starts"
    );
    let command = progress
        .commands
        .iter()
        .find(|entry| entry.command.contains("CHATOID-COMMAND-611"))
        .expect("Codex should report the command");
    assert_eq!(command.state, CommandState::Complete);
    assert_eq!(command.exit, Some(0));
    assert!(command.output.contains("CHATOID-COMMAND-611"));
    Ok(())
}

#[test]
#[ignore = "requires authenticated local Codex and a working workspace sandbox"]
fn real_codex_reads_small_and_staged_task_attachments() -> Result<()> {
    use chatoid::agent::app_server::{TaskInput, TaskThread, run_task};
    use std::sync::atomic::AtomicBool;
    let directory = tempfile::tempdir()?;
    for (padding, marker) in [(0, "FILE-6063"), (80 * 1024, "FILE-2074")] {
        let mut bytes = format!("{marker}\n").into_bytes();
        bytes.resize(bytes.len() + padding, b' ');
        let input = TaskInput {
            text: "Read the first line of the attachment provided by ChatOid. Return only the code on that line.",
            attachment: Some(("details.txt", &bytes)),
            web_research: false,
            network: false,
            delegate: false,
            steering: None,
            model: None,
            plan_only: false,
            review: None,
            skills: &[],
            compact_before: false,
        };
        let answer = run_task(
            &Settings::default(),
            directory.path(),
            input,
            TaskThread::Ephemeral,
            &AtomicBool::new(false),
            None,
            |_| {},
        )?
        .text;
        assert_eq!(answer.trim(), marker);
    }
    Ok(())
}

#[test]
#[ignore = "requires local Codex CLI; uses a simulated provider on loopback"]
fn codex_research_steering_and_cancellation_are_scoped_to_the_turn() -> Result<()> {
    use chatoid::agent::app_server::{ModelChoice, SteerRequest, TaskInput, TaskThread, run_task};
    use serde_json::json;
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
        time::{Duration, Instant},
    };

    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let (sent, received) = mpsc::channel();
    let (delivered, applied) = mpsc::channel();
    let (submit, instructions) = mpsc::sync_channel(1);
    let cancelled = Arc::new(AtomicBool::new(false));
    let interrupt = cancelled.clone();
    let provider = thread::spawn(move || -> Result<()> {
        let mut delivered = Some(delivered);
        for attempt in 0..5 {
            let deadline = Instant::now() + Duration::from_secs(15);
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
            stream.set_read_timeout(Some(Duration::from_secs(4)))?;
            let mut request = BufReader::new(&mut stream);
            let mut length = None;
            loop {
                let mut line = String::new();
                if request.read_line(&mut line)? == 0 || line.trim().is_empty() {
                    break;
                }
                if let Some((header, value)) = line.split_once(':')
                    && header.eq_ignore_ascii_case("content-length")
                {
                    length = Some(value.trim().parse::<usize>()?);
                }
            }
            let length = length.ok_or_else(|| anyhow::anyhow!("Missing fixture request length"))?;
            anyhow::ensure!(length < 2 * 1024 * 1024, "Unexpected fixture request size");
            let mut body = vec![0; length];
            request.read_exact(&mut body)?;
            if attempt == 3 {
                interrupt.store(true, Ordering::Release);
                // Hold the response open so the client's cancellation ends this turn.
                match request.read(&mut [0; 1]) {
                    Ok(0) => (),
                    Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => (),
                    other => {
                        anyhow::bail!("Codex did not close the interrupted request: {other:?}")
                    }
                };
                continue;
            }
            let body: Value = serde_json::from_slice(&body)?;
            if attempt == 4 {
                let history = body["input"].to_string();
                anyhow::ensure!(
                    history.contains("INTERRUPTED-STATE-312")
                        && history.contains("CONTINUE-ONCE-417"),
                    "Interrupted history was lost on resume"
                );
            }
            if attempt < 3 {
                anyhow::ensure!(
                    body["model"] == "gpt-5.1" && body["reasoning"]["effort"] == "high",
                    "Chosen model and effort were not propagated"
                );
            }
            let enabled = body["tools"].as_array().is_some_and(|tools| {
                tools.iter().any(|tool| {
                    matches!(
                        tool["type"].as_str(),
                        Some("web_search" | "web_search_preview")
                    )
                })
            });
            anyhow::ensure!(
                body["tools"]
                    .to_string()
                    .contains("\"name\":\"spawn_agent\"")
                    == enabled,
                "Delegation tools did not follow the turn's permission"
            );
            sent.send((
                enabled,
                body["input"].to_string().contains("STEERING-INPUT-491"),
            ))?;
            if attempt == 0 {
                submit.send(SteerRequest {
                    text: "STEERING-INPUT-491".into(),
                    delivered: delivered.take().expect("first request"),
                })?;
                anyhow::ensure!(
                    applied.recv_timeout(Duration::from_secs(10))?,
                    "Active turn refused steering"
                );
            }
            let mut events = vec![(
                "response.created",
                json!({"response":{"id":"resp_fixture","status":"in_progress","output":[]}}),
            )];
            let mut output = Vec::new();
            if attempt == 1 {
                let action = json!({"id":"ws_fixture", "type":"web_search_call", "status":"completed", "action":{"type":"open_page","url":"https://example.com/research"}});
                events.push((
                    "response.output_item.done",
                    json!({"output_index":0,"item":action}),
                ));
                output.push(action);
            }
            let answer = json!({"id":"msg_fixture","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"FIXTURE-ANSWER","annotations":[]}]});
            events.push((
                "response.output_item.done",
                json!({"output_index":output.len(),"item":answer}),
            ));
            output.push(answer);
            events.push((
                "response.completed",
                json!({"response":{"id":"resp_fixture","status":"completed","output":output}}),
            ));
            let mut response = String::new();
            for (kind, mut envelope) in events {
                envelope["type"] = json!(kind);
                response.push_str(&format!("event: {kind}\ndata: {envelope}\n\n"));
            }
            let written = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{response}",
                response.len()
            );
            if attempt != 0 {
                written?;
            }
        }
        Ok(())
    });

    let target = std::env::current_dir()?.join("target");
    std::fs::create_dir_all(&target)?;
    let root = tempfile::Builder::new()
        .prefix("research-")
        .tempdir_in(target)?;
    let home = root.path().join("codex");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&home)?;
    std::fs::create_dir(&workspace)?;
    std::fs::write(
        home.join("config.toml"),
        format!(
            r#"model = "gpt-5"
model_provider = "fixture"
[model_providers.fixture]
name = "ChatOid test provider"
base_url = "http://127.0.0.1:{port}/v1"
wire_api = "responses"
requires_openai_auth = false
"#
        ),
    )?;
    let choice = ModelChoice {
        model: "gpt-5.1".into(),
        effort: "high".into(),
    };
    let latest = std::cell::RefCell::new(chatoid::agent::app_server::TaskProgress::default());
    let task_text = std::cell::RefCell::new("Summarize the test source.");
    let run = |thread, web_research, steering, cancelled: &AtomicBool| {
        run_task(
            &Settings::default(),
            &workspace,
            TaskInput {
                text: &task_text.borrow(),
                attachment: None,
                web_research,
                network: web_research,
                delegate: web_research,
                steering,
                model: Some(&choice),
                plan_only: false,
                review: None,
                skills: &[],
                compact_before: false,
            },
            thread,
            cancelled,
            None,
            |progress| {
                latest.replace(progress);
            },
        )
    };
    let first = run(
        TaskThread::New(&home),
        true,
        Some(&instructions),
        &cancelled,
    )?;
    assert!(
        first.text.contains("https://example.com/research"),
        "{}",
        first.text
    );
    let thread_id = first.thread_id.as_deref().expect("private thread id");
    let next = run(
        TaskThread::Resume {
            home: &home,
            id: thread_id,
        },
        false,
        None,
        &cancelled,
    )?;
    task_text.replace("INTERRUPTED-STATE-312");
    let stopped = run(TaskThread::New(&home), false, None, &cancelled)
        .err()
        .expect("pending Codex turn should be interrupted");
    assert!(stopped.to_string().contains("iptal edildi"), "{stopped:#}");
    let checkpoint = latest
        .borrow()
        .session
        .clone()
        .expect("Private thread must be checkpointed before completion");
    assert_ne!(checkpoint, thread_id);
    cancelled.store(false, Ordering::Release);
    task_text.replace("CONTINUE-ONCE-417");
    let recovered = run(
        TaskThread::Resume {
            home: &home,
            id: &checkpoint,
        },
        false,
        None,
        &cancelled,
    )?;
    assert_eq!(recovered.thread_id.as_deref(), Some(checkpoint.as_str()));
    assert_eq!(recovered.text.trim(), "FIXTURE-ANSWER");
    provider.join().expect("fixture task")?;
    assert_eq!(
        received.recv_timeout(Duration::from_secs(1))?,
        (true, false)
    );
    assert_eq!(received.recv_timeout(Duration::from_secs(1))?, (true, true));
    assert!(!received.recv_timeout(Duration::from_secs(1))?.0);
    assert!(!received.recv_timeout(Duration::from_secs(1))?.0);
    assert_eq!(next.text.trim(), "FIXTURE-ANSWER");
    Ok(())
}

fn human() -> Result<Identity> {
    Identity::generate(Role::Human, "deniz", "Deniz")
}
fn agent_identity() -> Result<Identity> {
    Identity::generate(Role::Agent, "poyraz", "Poyraz")
}

fn host(owner: &Identity) -> Result<(TempDir, RunningHost, Connection)> {
    let temp = tempfile::tempdir()?;
    let bind = std::env::var("CHATOID_TEST_BIND").unwrap_or_else(|_| "127.0.0.1:0".into());
    let running = start(temp.path(), bind.parse()?, owner)?;
    let connection = Connection::new(running.address.clone())?;
    Ok((temp, running, connection))
}

fn redeem(connection: &mut Connection, actor: &Identity, ticket: &Ticket) -> Result<Request> {
    connection.call(
        actor,
        Action::Redeem {
            id: ticket.id,
            code: ticket.code.clone(),
            grant: ticket.grant.clone(),
        },
    )
}

fn decide(
    connection: &mut Connection,
    actor: &Identity,
    request: &Request,
    accepted: bool,
    grant: Option<Grant>,
) -> Result<Request> {
    connection.call(
        actor,
        Action::Decide {
            decision: Decision::issue(actor, request.terms.data.clone(), accepted, grant)?,
        },
    )
}

fn sync(connection: &mut Connection, actor: &Identity, cursor: i64) -> Result<Sync> {
    connection.call(
        actor,
        Action::Sync {
            after: cursor,
            grants: vec![],
            read: Default::default(),
            avatar: None,
        },
    )
}

fn draft(room: Uuid, text: &str, reply: Option<i64>) -> Draft {
    Draft {
        mode: None,
        id: Uuid::new_v4(),
        room,
        text: text.into(),
        reply,
        invitation: None,
        attachment: None,
    }
}

fn send(connection: &mut Connection, actor: &Identity, draft: Draft) -> Result<Message> {
    connection.call(actor, Action::Send { draft })
}

#[test]
fn full_size_binary_file_is_atomic_private_and_idempotent() -> Result<()> {
    use chatoid::{
        host::digest,
        protocol::{FILE_CHUNK, MAX_BINARY_ATTACHMENT},
    };
    let sender = human()?;
    let receiver = Identity::generate(Role::Human, "alici", "Alıcı")?;
    let outsider = Identity::generate(Role::Human, "yabanci", "Yabancı")?;
    let (_dir, runtime, mut author) = host(&sender)?;
    let mut guest = Connection::new(runtime.address.clone())?;
    let mut stranger = Connection::new(runtime.address.clone())?;
    let (_, ticket) = author.offer(&sender, receiver.card.clone(), None)?;
    let request = redeem(&mut guest, &receiver, &ticket)?;
    decide(&mut guest, &receiver, &request, true, None)?;
    let room = request.terms.data.id;
    let draft = draft(room, "", None);
    let chunk = vec![27; FILE_CHUNK];
    let bytes = MAX_BINARY_ATTACHMENT;
    let hash = digest(vec![27; bytes]);
    let commit = || Action::SendBinary {
        draft: draft.clone(),
        name: "dosya.bin".into(),
        bytes,
        hash: hash.clone(),
    };
    author.upload_chunk(&sender, draft.id, 0, &chunk)?;
    assert!(author.call::<Message>(&sender, commit()).is_err());
    assert!(sync(&mut guest, &receiver, 0)?.messages.is_empty());
    for offset in (0..bytes).step_by(FILE_CHUNK) {
        author.upload_chunk(&sender, draft.id, offset, &chunk)?;
    }
    let message: Message = author.call(&sender, commit())?;
    assert!(message.attachment.as_ref().unwrap().binary);
    assert_eq!(author.call::<Message>(&sender, commit())?.seq, message.seq);
    assert!(stranger.download_chunk(&outsider, message.seq, 0).is_err());
    for offset in (0..bytes).step_by(FILE_CHUNK) {
        assert_eq!(guest.download_chunk(&receiver, message.seq, offset)?, chunk);
    }
    Ok(())
}

#[test]
fn single_contact_link_delivers_and_ending_friendship_invalidates_old_proof() -> Result<()> {
    use chatoid::protocol::{ContactRoute, INVITE_LIFETIME, now};
    let sender = human()?;
    let recipient = Identity::generate(Role::Human, "duru", "Duru")?;
    let (_authority_dir, authority, mut source) = host(&sender)?;
    let (_target_dir, target, mut target_owner) = host(&recipient)?;
    let route = recipient.sign(
        "contact-route",
        ContactRoute {
            card: recipient.card.clone(),
            host: target.address.clone(),
            expires: now() + INVITE_LIFETIME,
        },
    )?;
    let (_, ticket) = source.offer(&sender, recipient.card.clone(), None)?;
    let mut relay = Connection::new(target.address.clone())?;
    for _ in 0..2 {
        relay.call::<Value>(
            &sender,
            Action::DeliverIntroduction {
                route: Box::new(route.clone()),
                ticket: ticket.clone(),
            },
        )?;
    }
    let inbox = sync(&mut target_owner, &recipient, 0)?.introductions;
    assert_eq!(inbox.len(), 1);
    let mut guest = Connection::new(authority.address.clone())?;
    let requested = redeem(&mut guest, &recipient, &inbox[0])?;
    let proof = decide(&mut guest, &recipient, &requested, true, None)?
        .friendship()
        .unwrap();
    let dm = requested.terms.data.id;
    send(&mut source, &sender, draft(dm, "Geçmiş kalır", None))?;
    let group = Uuid::new_v4();
    target_owner.call::<Uuid>(
        &recipient,
        Action::Group {
            id: group,
            name: "Ekip".into(),
        },
    )?;
    let (_, invite) = target_owner.offer_with_authority(
        &recipient,
        sender.card.clone(),
        Some((group, proof.clone())),
        Some(authority.address.clone()),
    )?;
    let mut group_guest = Connection::new(target.address.clone())?;
    let group_pending = redeem(&mut group_guest, &sender, &invite)?;
    guest.call::<Value>(&recipient, Action::EndFriendship { room: dm })?;
    assert!(
        sync(&mut source, &sender, 0)?
            .rooms
            .iter()
            .any(|room| room.id == dm && room.closed)
    );
    assert_eq!(sync(&mut guest, &recipient, 0)?.messages.len(), 1);
    assert!(send(&mut source, &sender, draft(dm, "Reddedilir", None)).is_err());
    assert!(decide(&mut group_guest, &sender, &group_pending, true, None).is_err());
    assert!(!source.friendship_active(&proof)?);
    let (_, again) = source.offer(&sender, recipient.card.clone(), None)?;
    assert_ne!(again.id, dm);
    Ok(())
}

#[test]
fn agent_device_summary_is_returned_only_to_active_friends() -> Result<()> {
    use chatoid::protocol::{DeviceSummary, ProfilePublication, PublicProfile};
    let human = human()?;
    let agent = agent_identity()?;
    let outsider = Identity::generate(Role::Human, "konuk", "Konuk")?;
    let (_dir, running, mut owner) = host(&human)?;
    let mut guest = Connection::new(running.address.clone())?;
    let mut unrelated = Connection::new(running.address.clone())?;
    let (profile, device) = (
        agent.sign(
            "shared-profile",
            PublicProfile {
                display_name: "Poyraz".into(),
                agent: None,
            },
        )?,
        agent.sign(
            "device-summary",
            DeviceSummary {
                system: "Windows 11".into(),
                architecture: "x86_64".into(),
                processor: "8 mantıksal işlemci".into(),
                memory: "16 GiB".into(),
            },
        )?,
    );
    guest.call::<Value>(
        &agent,
        Action::PublishProfile {
            publication: Box::new(ProfilePublication {
                profile,
                device: Some(device),
            }),
        },
    )?;
    let load = || Action::LoadProfile {
        actor: agent.id().into(),
    };
    assert!(
        unrelated
            .call::<Option<ProfilePublication>>(&outsider, load())
            .is_err()
    );
    let (_, invite) = owner.offer(&human, agent.card.clone(), None)?;
    let request = redeem(&mut guest, &agent, &invite)?;
    let proof = decide(&mut guest, &agent, &request, true, None)?
        .friendship()
        .unwrap();
    let info = owner
        .call::<Option<ProfilePublication>>(&human, load())?
        .unwrap();
    assert_eq!(info.device.unwrap().data.memory, "16 GiB");
    let group = Uuid::new_v4();
    owner.call::<Uuid>(
        &human,
        Action::Group {
            id: group,
            name: "Ortak grup".into(),
        },
    )?;
    let (_, ticket) = owner.offer(&human, agent.card.clone(), Some((group, proof)))?;
    let pending = redeem(&mut guest, &agent, &ticket)?;
    decide(&mut guest, &agent, &pending, true, None)?;
    owner.call::<Value>(
        &human,
        Action::EndFriendship {
            room: request.terms.data.id,
        },
    )?;
    let info = owner
        .call::<Option<ProfilePublication>>(&human, load())?
        .unwrap();
    assert!(info.device.is_none());
    Ok(())
}

#[test]
fn files_photos_and_receipts_are_visible_only_in_shared_chats() -> Result<()> {
    let sender = human()?;
    let receiver = Identity::generate(Role::Human, "yagmur", "Yağmur")?;
    let outsider = Identity::generate(Role::Human, "misafir", "Misafir")?;
    let (_directory, running, mut author) = host(&sender)?;
    let mut guest = Connection::new(running.address.clone())?;
    let mut stranger = Connection::new(running.address.clone())?;
    let (_, invite) = author.offer(&sender, receiver.card.clone(), None)?;
    let request = redeem(&mut guest, &receiver, &invite)?;
    decide(&mut guest, &receiver, &request, true, None)?;
    let room = request.terms.data.id;

    let image = image::RgbImage::from_pixel(AVATAR_PX, AVATAR_PX, image::Rgb([55, 142, 140]));
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut jpeg).encode_image(&image)?;
    let data = encode(&jpeg);
    let digest = AvatarPhoto::digest(&data);
    author.call::<Sync>(
        &sender,
        Action::Sync {
            after: 0,
            grants: vec![],
            read: Default::default(),
            avatar: Some(AvatarUpdate {
                data: Some(data.clone()),
            }),
        },
    )?;
    assert!(sync(&mut stranger, &outsider, 0)?.avatar_index.is_empty());
    assert_eq!(
        sync(&mut guest, &receiver, 0)?.avatar_index[sender.id()],
        digest
    );
    let request_photo = || Action::Avatar {
        actor: sender.id().into(),
        digest: digest.clone(),
    };
    assert!(
        stranger
            .call::<AvatarPhoto>(&outsider, request_photo())
            .is_err()
    );
    assert_eq!(
        guest.call::<AvatarPhoto>(&receiver, request_photo())?.data,
        data
    );

    let mut outgoing = draft(room, "", None);
    outgoing.attachment = Some(Attachment {
        name: "örnek.jpg".into(),
        data,
    });
    let message = send(&mut author, &sender, outgoing.clone())?;
    assert_eq!(
        send(&mut author, &sender, outgoing.clone())?.seq,
        message.seq
    );
    outgoing.attachment.as_mut().unwrap().name = "başka.jpg".into();
    assert!(send(&mut author, &sender, outgoing).is_err());
    let received = sync(&mut guest, &receiver, 0)?;
    let file = received.messages[0].attachment.as_ref().unwrap();
    assert_eq!(file.name, "örnek.jpg");
    assert_eq!(file.bytes, jpeg.len());
    assert_eq!(file.preview.as_ref().unwrap().decode()?.width(), AVATAR_PX);
    let fetch = || Action::Download {
        room,
        seq: message.seq,
    };
    assert!(stranger.call::<Attachment>(&outsider, fetch()).is_err());
    assert!(
        guest
            .call::<Attachment>(
                &receiver,
                Action::Download {
                    room: Uuid::new_v4(),
                    seq: message.seq,
                }
            )
            .is_err()
    );
    assert_eq!(guest.call::<Attachment>(&receiver, fetch())?.bytes()?, jpeg);

    assert_eq!(sync(&mut author, &sender, 0)?.receipts[0].delivered, 0);
    sync(&mut guest, &receiver, received.cursor)?;
    let delivered = sync(&mut author, &sender, 0)?;
    assert_eq!(delivered.receipts[0].delivered, message.seq);
    assert_eq!(delivered.receipts[0].read, 0);
    guest.call::<Sync>(
        &receiver,
        Action::Sync {
            after: received.cursor,
            grants: vec![],
            read: [(room, message.seq)].into(),
            avatar: None,
        },
    )?;
    assert_eq!(sync(&mut author, &sender, 0)?.receipts[0].read, message.seq);
    assert!(sync(&mut stranger, &outsider, 0)?.receipts.is_empty());
    Ok(())
}

#[test]
fn local_keys_and_role_separation() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let human = human()?;
    let agent = agent_identity()?;
    human.save(
        &temp.path().join("deniz.identity"),
        Some("local-test-passphrase"),
    )?;
    agent.save(&temp.path().join("poyraz.key"), None)?;
    assert_eq!(
        Identity::load(
            &temp.path().join("deniz.identity"),
            Role::Human,
            Some("local-test-passphrase")
        )?
        .id(),
        human.id()
    );
    assert_eq!(
        Identity::load(&temp.path().join("poyraz.key"), Role::Agent, None)?.id(),
        agent.id()
    );
    assert!(
        Identity::load(
            &temp.path().join("deniz.identity"),
            Role::Human,
            Some("wrong password")
        )
        .is_err()
    );
    assert!(
        Identity::load(
            &temp.path().join("poyraz.key"),
            Role::Human,
            Some("local-test-passphrase")
        )
        .is_err()
    );
    assert!(Identity::load(&temp.path().join("deniz.identity"), Role::Agent, None).is_err());
    Ok(())
}

#[test]
fn pinned_https_friendship_agent_operator_and_restart() -> Result<()> {
    let human = human()?;
    let agent = agent_identity()?;
    let operator = Identity::generate(Role::Human, "elya", "Elya")?;
    let (directory, host, mut sender) = host(&human)?;
    let address = host.address.clone();
    let mut recipient = Connection::new(address.clone())?;
    let mut observer = Connection::new(address.clone())?;
    let (_, mut ticket) = sender.offer(&human, agent.card.clone(), None)?;
    assert!(sender.offer(&human, agent.card.clone(), None).is_err());
    assert!(redeem(&mut observer, &operator, &ticket).is_err());
    let request = redeem(&mut recipient, &agent, &ticket)?;
    let grant = Grant::issue(&agent, operator.card.clone(), &request.terms.data)?;
    ticket.grant = Some(grant.clone());
    redeem(&mut observer, &operator, &ticket)?;
    let (one, two) = std::thread::scope(|scope| {
        let a = scope.spawn(|| decide(&mut recipient, &agent, &request, true, None));
        let b = scope.spawn(|| decide(&mut observer, &operator, &request, true, Some(grant)));
        (a.join().unwrap(), b.join().unwrap())
    });
    let (one, two) = (one?, two?);
    assert_eq!(one.status, Status::Accepted);
    assert_eq!(one.decision, two.decision);
    let room = ticket.id;
    let first = draft(room, "Kurulum hazır mı?", None);
    let receipt = send(&mut sender, &human, first.clone())?;
    assert_eq!(send(&mut sender, &human, first.clone())?.seq, receipt.seq);
    let reply = send(
        &mut recipient,
        &agent,
        draft(room, "Hazır.", Some(receipt.seq)),
    )?;
    assert_eq!(reply.reply.as_ref().unwrap().id, receipt.seq);
    assert_eq!(sync(&mut recipient, &agent, 0)?.messages.len(), 2);
    assert!(sync(&mut observer, &operator, 0)?.messages.is_empty());
    assert!(
        observer
            .call::<Vec<Message>>(
                &operator,
                Action::History {
                    room,
                    before: i64::MAX
                }
            )
            .is_err()
    );
    let mut wrong = address.clone();
    wrong.pin = chatoid::identity::encode([9; 32]);
    assert!(sync(&mut Connection::new(wrong)?, &human, 0).is_err());
    drop(host);
    std::thread::sleep(std::time::Duration::from_millis(350));
    let _restarted = start(directory.path(), address.socket, &human)?;
    assert_eq!(send(&mut sender, &human, first)?.seq, receipt.seq);
    assert_eq!(sync(&mut sender, &human, 0)?.messages.len(), 2);
    Ok(())
}

#[test]
fn cross_host_group_requires_separate_acceptance_and_enforces_history() -> Result<()> {
    let human = human()?;
    let agent = agent_identity()?;
    let (_first_dir, first, mut agent_host) = host(&agent)?;
    let mut human_guest = Connection::new(first.address.clone())?;
    let (_, ticket) = agent_host.offer(&agent, human.card.clone(), None)?;
    let friend = redeem(&mut human_guest, &human, &ticket)?;
    let accepted = decide(&mut human_guest, &human, &friend, true, None)?;
    let friendship = accepted.friendship().unwrap();
    let authority = first.address.clone();

    let (second_dir, second, mut human_host) = host(&human)?;
    let mut agent_guest = Connection::new(second.address.clone())?;
    let room = Uuid::new_v4();
    assert_eq!(
        human_host.call::<Uuid>(
            &human,
            Action::Group {
                id: room,
                name: "Proje ekibi".into()
            }
        )?,
        room
    );
    assert!(
        agent_guest
            .call::<Uuid>(
                &agent,
                Action::Group {
                    id: Uuid::new_v4(),
                    name: "Yetkisiz".into()
                }
            )
            .is_err()
    );
    let before = send(&mut human_host, &human, draft(room, "Üyelik öncesi", None))?;
    let (_, invite) = human_host.offer_with_authority(
        &human,
        agent.card.clone(),
        Some((room, friendship.clone())),
        Some(authority.clone()),
    )?;
    let pending = redeem(&mut agent_guest, &agent, &invite)?;
    assert!(sync(&mut agent_guest, &agent, 0)?.rooms.is_empty());
    assert!(
        send(
            &mut agent_guest,
            &agent,
            draft(room, "Onaysız mesaj", Some(before.seq))
        )
        .is_err()
    );
    assert_eq!(
        decide(&mut agent_guest, &agent, &pending, false, None)?.status,
        Status::Rejected
    );
    assert!(sync(&mut agent_guest, &agent, 0)?.rooms.is_empty());
    let (_, invite) = human_host.offer_with_authority(
        &human,
        agent.card.clone(),
        Some((room, friendship)),
        Some(authority),
    )?;
    let pending = redeem(&mut agent_guest, &agent, &invite)?;
    decide(&mut agent_guest, &agent, &pending, true, None)?;
    let after = send(
        &mut human_host,
        &human,
        draft(
            room,
            "Poyraz, kurulumu kontrol eder misin?",
            Some(before.seq),
        ),
    )?;
    let state = sync(&mut agent_guest, &agent, 0)?;
    assert_eq!(state.messages.len(), 1);
    assert_eq!(state.messages[0].seq, after.seq);
    assert!(state.messages[0].reply.is_none());
    assert_eq!(state.rooms[0].owner, human.id());
    assert!(
        send(
            &mut agent_guest,
            &agent,
            draft(room, "Yasak alıntı", Some(before.seq))
        )
        .is_err()
    );
    let reply = send(
        &mut agent_guest,
        &agent,
        draft(room, "Kurulum tamam.", Some(after.seq)),
    )?;
    assert_eq!(reply.root, Some(after.seq));
    let _: Value = human_host.call(
        &human,
        Action::Remove {
            room,
            member: agent.id().into(),
        },
    )?;
    send(&mut human_host, &human, draft(room, "Üyelik sonrası", None))?;
    let state = sync(&mut agent_guest, &agent, 0)?;
    assert!(!state.rooms[0].active);
    assert!(state.rooms[0].members.is_empty());
    assert_eq!(state.messages.len(), 2);
    assert!(
        send(
            &mut agent_guest,
            &agent,
            draft(room, "Yetkisiz mesaj", Some(after.seq))
        )
        .is_err()
    );
    let address = second.address.clone();
    drop(second);
    let restarted = start(second_dir.path(), address.socket, &human)?;
    let mut host_client = Connection::new(restarted.address.clone())?;
    let persisted = sync(&mut host_client, &human, 0)?;
    assert_eq!(persisted.rooms[0].id, room);
    assert_eq!(persisted.rooms[0].owner, human.id());
    Ok(())
}

#[test]
fn group_agent_answers_have_a_shared_finite_chain() -> Result<()> {
    let human = human()?;
    let (_directory, running, mut server) = host(&human)?;
    let room = Uuid::new_v4();
    let _: Uuid = server.call(
        &human,
        Action::Group {
            id: room,
            name: "Ajanlar".into(),
        },
    )?;
    let mut agents = Vec::new();
    for handle in ["poyraz", "kivilcim", "denizci", "yildiz"] {
        let member = Identity::generate(Role::Agent, handle, handle)?;
        let mut client = Connection::new(running.address.clone())?;
        let (_, invite) = server.offer(&human, member.card.clone(), None)?;
        let friend = redeem(&mut client, &member, &invite)?;
        let proof = decide(&mut client, &member, &friend, true, None)?
            .friendship()
            .unwrap();
        let (_, invite) = server.offer(&human, member.card.clone(), Some((room, proof)))?;
        let pending = redeem(&mut client, &member, &invite)?;
        decide(&mut client, &member, &pending, true, None)?;
        agents.push((member, client));
    }
    let root = send(
        &mut server,
        &human,
        draft(room, "@poyraz başlayabilir misin?", None),
    )?;
    let state = sync(&mut server, &human, 0)?;
    let group = state.rooms.iter().find(|r| r.id == room).unwrap();
    assert_eq!(
        agent::addressed(group, &root, &agents[0].0.card.data),
        Some(true)
    );
    assert_eq!(agent::addressed(group, &root, &agents[1].0.card.data), None);
    let mut parent = root.seq;
    for (index, next) in [
        "@kivilcim devam et",
        "@denizci devam et",
        "@yildiz devam et",
    ]
    .iter()
    .enumerate()
    {
        let (actor, client) = &mut agents[index];
        let reply = send(client, actor, draft(room, next, Some(parent)))?;
        assert_eq!(reply.root, Some(root.seq));
        parent = reply.seq;
    }
    let (actor, client) = &mut agents[3];
    assert!(send(client, actor, draft(room, "Dördüncü yanıt", Some(parent))).is_err());
    let state = sync(&mut server, &human, root.seq)?;
    assert_eq!(
        state
            .messages
            .iter()
            .filter(|m| m.root == Some(root.seq))
            .count(),
        3
    );
    Ok(())
}

#[test]
#[ignore = "requires an authenticated local Codex CLI and provider access"]
fn real_codex_natural_address_and_workspace_read() -> Result<()> {
    let human = human()?;
    let agent = agent_identity()?;
    let (_directory, running, mut client) = host(&human)?;
    let mut recipient = Connection::new(running.address.clone())?;
    let (_, ticket) = client.offer(&human, agent.card.clone(), None)?;
    let request = redeem(&mut recipient, &agent, &ticket)?;
    let friend = decide(&mut recipient, &agent, &request, true, None)?
        .friendship()
        .unwrap();
    let room = Uuid::new_v4();
    let _: Uuid = client.call(
        &human,
        Action::Group {
            id: room,
            name: "Kurulum ekibi".into(),
        },
    )?;
    let (_, invite) = client.offer(&human, agent.card.clone(), Some((room, friend)))?;
    let request = redeem(&mut recipient, &agent, &invite)?;
    decide(&mut recipient, &agent, &request, true, None)?;
    let incoming = send(
        &mut client,
        &human,
        draft(
            room,
            "Poyraz, lütfen README.md dosyasını incele ve oradaki kurulum kodunu yaz.",
            None,
        ),
    )?;
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("README.md"),
        "Windows kurulum kodu: TURKUAZ-6183",
    )?;
    let settings = Settings {
        workspace: directory.path().to_string_lossy().into_owned(),
        permits: vec![FilePermit {
            actor: human.id().into(),
            host: running.address.pin.clone(),
            room,
        }],
        ..Settings::default()
    };
    let state = sync(&mut recipient, &agent, 0)?;
    let group = state.rooms.iter().find(|r| r.id == room).unwrap();
    agent::check(&settings)?;
    let answer = agent::response(
        &settings,
        &running.address.pin,
        &agent.card.data,
        group,
        &state.messages,
        &incoming,
    )?
    .expect("the agent should answer the direct request");
    assert!(
        answer.contains("TURKUAZ-6183"),
        "The answer must include the value only visible in the permitted file"
    );
    let reply = send(
        &mut recipient,
        &agent,
        draft(room, &answer, Some(incoming.seq)),
    )?;
    assert_eq!(reply.reply.as_ref().unwrap().id, incoming.seq);
    Ok(())
}

#[test]
fn message_modes_and_half_second_withdrawal_are_authorized_and_redacted() -> Result<()> {
    use chatoid::protocol::MessageMode;
    let sender = human()?;
    let receiver = agent_identity()?;
    let outsider = Identity::generate(Role::Human, "outsider", "Outsider")?;
    let (_dir, runtime, mut source) = host(&sender)?;
    let mut guest = Connection::new(runtime.address.clone())?;
    let (_, ticket) = source.offer(&sender, receiver.card.clone(), None)?;
    let request = redeem(&mut guest, &receiver, &ticket)?;
    decide(&mut guest, &receiver, &request, true, None)?;
    let room = request.terms.data.id;
    let mut stranger = Connection::new(runtime.address.clone())?;
    sync(&mut stranger, &outsider, 0)?;
    let mut body = draft(room, "Yalnızca bu yönerge", None);
    body.mode = Some(MessageMode::Goal);
    body.attachment = Some(Attachment {
        name: "note.txt".into(),
        data: encode(b"private fixture"),
    });
    let original = body.clone();
    let posted = send(&mut source, &sender, body)?;
    assert_eq!(posted.text, "Yalnızca bu yönerge");
    assert_eq!(posted.mode, Some(MessageMode::Goal));
    assert!(!guest.call::<bool>(
        &receiver,
        Action::CanProcessMessage {
            room,
            seq: posted.seq
        }
    )?);
    assert!(
        stranger
            .call::<Value>(
                &outsider,
                Action::WithdrawMessage {
                    room,
                    seq: posted.seq
                }
            )
            .is_err()
    );
    let reply = send(
        &mut guest,
        &receiver,
        draft(room, "Alıntı", Some(posted.seq)),
    )?;
    source.call::<Value>(
        &sender,
        Action::WithdrawMessage {
            room,
            seq: posted.seq,
        },
    )?;
    source.call::<Value>(
        &sender,
        Action::WithdrawMessage {
            room,
            seq: posted.seq,
        },
    )?;
    let repeated = send(&mut source, &sender, original)?;
    assert!(repeated.withdrawn);
    assert!(repeated.text.is_empty());
    let synced = sync(&mut guest, &receiver, 0)?;
    assert!(synced.supports_message_modes);
    assert!(synced.withdrawn_messages.contains(&posted.seq));
    let tombstone = synced
        .messages
        .iter()
        .find(|m| m.seq == posted.seq)
        .unwrap();
    assert!(
        tombstone.withdrawn
            && tombstone.text.is_empty()
            && tombstone.attachment.is_none()
            && tombstone.mode.is_none()
    );
    assert!(
        synced
            .messages
            .iter()
            .find(|m| m.seq == reply.seq)
            .unwrap()
            .reply
            .is_none()
    );
    assert!(
        sync(&mut stranger, &outsider, 0)?
            .withdrawn_messages
            .is_empty()
    );
    let mut bytes = Vec::new();
    assert!(
        guest
            .download_attachment(&receiver, &posted, &mut bytes, || Ok(()))
            .is_err()
    );
    std::thread::sleep(std::time::Duration::from_millis(550));
    assert!(!guest.call::<bool>(
        &receiver,
        Action::CanProcessMessage {
            room,
            seq: posted.seq
        }
    )?);
    let late = send(
        &mut source,
        &sender,
        draft(room, "Süresi geçen mesaj", None),
    )?;
    std::thread::sleep(std::time::Duration::from_millis(550));
    assert!(
        source
            .call::<Value>(
                &sender,
                Action::WithdrawMessage {
                    room,
                    seq: late.seq
                }
            )
            .is_err()
    );
    assert!(guest.call::<bool>(
        &receiver,
        Action::CanProcessMessage {
            room,
            seq: late.seq
        }
    )?);
    assert!(
        source
            .call::<bool>(
                &sender,
                Action::CanProcessMessage {
                    room,
                    seq: late.seq
                }
            )
            .is_err()
    );
    Ok(())
}
