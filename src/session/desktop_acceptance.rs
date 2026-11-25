use super::*;
use std::{
    os::windows::process::CommandExt,
    process::{Child, Command as ProcessCommand, Stdio},
};

use base64::{Engine, engine::general_purpose::STANDARD};

struct Fixture(Child);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn windows_fixture(script: &str) -> ProcessCommand {
    let encoded = STANDARD.encode(
        script
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    );
    let mut command = ProcessCommand::new("powershell.exe");
    command
        .args(["-NoProfile", "-STA", "-EncodedCommand", &encoded])
        .creation_flags(0x08000000)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

#[test]
#[ignore = "requires the interactive Windows desktop; no Codex login needed"]
fn approved_window_switch_routes_capture_and_controls_to_the_second_window() -> Result<()> {
    use crate::agent::{app_server::TaskResult, desktop_tools, task_tools::TaskTools};
    use serde_json::json;
    use std::sync::atomic::AtomicBool;

    let token = Uuid::new_v4();
    let title_a = format!("Window routing fixture red {token}");
    let title_b = format!("Window routing fixture blue {token}");
    let workspace = tempfile::tempdir()?;
    let marker = workspace.path().join("invoked");
    let rename = workspace.path().join("rename");
    let renamed = workspace.path().join("renamed");
    let script = r#"
        Add-Type -AssemblyName System.Windows.Forms
        Add-Type -AssemblyName System.Drawing
        $one = New-Object Windows.Forms.Form
        $one.Text = $env:FIXTURE_TITLE_A
        $one.StartPosition = 'Manual'
        $one.Location = [Drawing.Point]::new(100,100)
        $one.ClientSize = [Drawing.Size]::new(280,160)
        $one.BackColor = [Drawing.Color]::Crimson
        $redButton = New-Object Windows.Forms.Button
        $redButton.Text = 'Red button fixture'
        $redButton.SetBounds(6,7,120,27)
        $one.Controls.Add($redButton)
        $two = New-Object Windows.Forms.Form
        $two.Text = $env:FIXTURE_TITLE_B
        $two.StartPosition = 'Manual'
        $two.Location = [Drawing.Point]::new(440,100)
        $two.ClientSize = [Drawing.Size]::new(280,160)
        $two.BackColor = [Drawing.Color]::DodgerBlue
        $blueButton = New-Object Windows.Forms.Button
        $blueButton.Text = 'Blue button fixture'
        $blueButton.SetBounds(6,7,120,27)
        $blueButton.Add_Click({ [IO.File]::AppendAllText($env:FIXTURE_INVOKED, '1') })
        $timer = New-Object Windows.Forms.Timer
        $timer.Interval = 40
        $timer.Add_Tick({
            if ([IO.File]::Exists($env:FIXTURE_RENAME)) {
                $blueButton.Text = 'Updated button fixture'
                [IO.File]::WriteAllText($env:FIXTURE_RENAMED, 'ok')
                $timer.Stop()
            }
        })
        $timer.Start()
        $two.Controls.Add($blueButton)
        $masked = New-Object Windows.Forms.TextBox
        $masked.UseSystemPasswordChar = $true
        $masked.AccessibleName = 'Masked fixture field'
        $masked.Text = 'Synthetic secret 0429'
        $masked.SetBounds(6,42,120,27)
        $two.Controls.Add($masked)
        $one.Add_Shown({ $two.Show() })
        [Windows.Forms.Application]::Run($one)
    "#;
    let _fixture = Fixture(
        windows_fixture(script)
            .env("FIXTURE_TITLE_A", &title_a)
            .env("FIXTURE_TITLE_B", &title_b)
            .env("FIXTURE_INVOKED", &marker)
            .env("FIXTURE_RENAME", &rename)
            .env("FIXTURE_RENAMED", &renamed)
            .spawn()?,
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let (first, second) = loop {
        let windows = desktop_tools::windows();
        if let (Some(first), Some(second)) = (
            windows.iter().find(|target| target.title == title_a),
            windows.iter().find(|target| target.title == title_b),
        ) {
            break (first.clone(), second.clone());
        }
        ensure!(Instant::now() < deadline, "Test windows did not appear");
        thread::sleep(Duration::from_millis(30));
    };

    let tools = Arc::new(TaskTools::new(
        workspace.path().to_path_buf(),
        0,
        Some(first.clone()),
    ));
    let model = tools.clone();
    let (sender, receiver) = mpsc::channel::<Result<()>>();
    let worker = thread::spawn(move || {
        let run = || -> Result<()> {
            let cancelled = AtomicBool::new(false);
            let mut result = TaskResult::default();
            let capture =
                json!({"tool":"chatoid_window_image","arguments":{"share_in_chat":false}});
            let rgb = |event: Value| -> Result<[u8; 3]> {
                let url = event["contentItems"][0]["imageUrl"]
                    .as_str()
                    .context("Expected image output")?;
                let bytes = STANDARD.decode(
                    url.strip_prefix("data:image/jpeg;base64,")
                        .context("Expected JPEG")?,
                )?;
                let picture = image::load_from_memory(&bytes)?.to_rgb8();
                Ok(picture
                    .get_pixel(picture.width() / 2, picture.height() / 2)
                    .0)
            };
            let first_pixel = rgb(desktop_tools::call(
                &model,
                &capture,
                &mut result,
                &cancelled,
            )?)?;
            assert!(
                i16::from(first_pixel[0]) - i16::from(first_pixel[2]) > 80,
                "{first_pixel:?}"
            );
            let select = json!({"tool":"chatoid_choose_window","arguments":{"reason":"Read the separate blue window"}});
            assert_eq!(
                desktop_tools::call(&model, &select, &mut result, &cancelled)?["success"],
                false
            );
            assert_eq!(model.window(), Some(first));
            assert_eq!(
                desktop_tools::call(&model, &select, &mut result, &cancelled)?["success"],
                true
            );
            let second_pixel = rgb(desktop_tools::call(
                &model,
                &capture,
                &mut result,
                &cancelled,
            )?)?;
            assert!(
                i16::from(second_pixel[2]) - i16::from(second_pixel[0]) > 80,
                "{second_pixel:?}"
            );
            assert!(result.screenshot.is_none());
            let controls = json!({"tool":"chatoid_window_controls","arguments":{}});
            assert!(desktop_tools::call(&model, &controls, &mut result, &cancelled).is_err());
            let read = desktop_tools::call(&model, &controls, &mut result, &cancelled)?;
            let text = read["contentItems"][0]["text"]
                .as_str()
                .context("Expected control data")?;
            assert!(
                !text.contains("Red button fixture")
                    && !text.contains("Synthetic secret 0429")
                    && !text.contains("Masked fixture field")
            );
            let data: Value = serde_json::from_str(text)?;
            let blue = data["controls"]
                .as_array()
                .context("Missing control list")?
                .iter()
                .find(|entry| entry["label"] == "Blue button fixture")
                .context("Blue button missing")?;
            assert!(blue["x"].as_u64().is_some_and(|x| x < 500));
            let original = blue["button_id"].as_str().context("Button ID missing")?;
            let stale = json!({"tool":"chatoid_window_invoke","arguments":{"button_id":original}});
            assert!(desktop_tools::call(&model, &stale, &mut result, &cancelled).is_err());
            let read = desktop_tools::call(&model, &controls, &mut result, &cancelled)?;
            let data: Value = serde_json::from_str(
                read["contentItems"][0]["text"]
                    .as_str()
                    .context("Missing response")?,
            )?;
            let updated = data["controls"]
                .as_array()
                .context("Missing control list")?
                .iter()
                .find(|entry| entry["label"] == "Updated button fixture")
                .context("Updated button missing")?;
            let id = updated["button_id"]
                .as_str()
                .context("Updated button ID missing")?;
            assert_ne!(original, id);
            let invoke = json!({"tool":"chatoid_window_invoke","arguments":{"button_id":id}});
            assert!(desktop_tools::call(&model, &invoke, &mut result, &cancelled).is_err());
            assert_eq!(
                desktop_tools::call(&model, &invoke, &mut result, &cancelled)?["success"],
                true
            );
            assert!(desktop_tools::call(&model, &invoke, &mut result, &cancelled).is_err());
            Ok(())
        };
        let _ = sender.send(run());
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut rejected = false;
    let mut chosen = false;
    let mut observed = Vec::new();
    let mut declined_read = false;
    let mut declined_click = false;
    loop {
        if let Some(request) = tools.window_request() {
            assert_eq!(request.reason, "Read the separate blue window");
            assert!(
                tools
                    .choose_window(Uuid::new_v4(), Some(second.clone()))
                    .is_err()
            );
            if rejected {
                tools.choose_window(request.id, Some(second.clone()))?;
                chosen = true;
            } else {
                tools.choose_window(request.id, None)?;
                rejected = true;
            }
        }
        if let Some(approval) = tools.pending() {
            observed.push(approval.title.clone());
            let decline_read = !declined_read && approval.action.starts_with("Pencerenin görünür");
            let decline_click =
                !declined_click && approval.action == "Düğmeyi çalıştır · Updated button fixture";
            if approval.action == "Düğmeyi çalıştır · Blue button fixture" {
                fs::write(&rename, "rename")?;
                let deadline = Instant::now() + Duration::from_secs(3);
                while !renamed.exists() {
                    ensure!(
                        Instant::now() < deadline,
                        "Fixture did not rename the button"
                    );
                    thread::sleep(Duration::from_millis(20));
                }
            }
            declined_read |= decline_read;
            declined_click |= decline_click;
            tools.decide(approval.id, !(decline_read || decline_click))?;
        }
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(result) => {
                result?;
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
            Err(error) => return Err(error.into()),
        }
    }
    worker.join().expect("desktop tool worker");
    assert!(rejected && chosen && declined_read && declined_click);
    assert_eq!(observed[0], title_a);
    assert_eq!(observed.len(), 8);
    assert!(observed[1..].iter().all(|title| *title == title_b));
    let deadline = Instant::now() + Duration::from_secs(3);
    while !marker.exists() {
        ensure!(Instant::now() < deadline, "Approved button was not invoked");
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(fs::read_to_string(marker)?, "1");
    Ok(())
}

#[test]
#[ignore = "requires a visible Windows desktop and a local Codex login"]
fn real_codex_reads_and_controls_an_approved_test_window() -> anyhow::Result<()> {
    use crate::agent::desktop_tools;

    let temp = tempfile::tempdir()?;
    let click_proof = temp.path().join("clicked.txt");
    let scroll_proof = temp.path().join("scrolled.txt");
    let input_image = temp.path().join("input.png");
    let geometry_file = temp.path().join("geometry.json");
    let title = format!("Window fixture {}", Uuid::new_v4());
    let script = r#"
        Add-Type -AssemblyName System.Windows.Forms
        Add-Type -AssemblyName System.Drawing
        $bitmap = New-Object Drawing.Bitmap(660,150)
        $canvas = [Drawing.Graphics]::FromImage($bitmap)
        $canvas.Clear([Drawing.Color]::White)
        $font = New-Object Drawing.Font('Arial',38)
        $canvas.DrawString('IMAGE-8820', $font, [Drawing.Brushes]::Black, [single]22, [single]28)
        $bitmap.Save($env:FIXTURE_INPUT_IMAGE, [Drawing.Imaging.ImageFormat]::Png)
        $font.Dispose()
        $canvas.Dispose()
        $bitmap.Dispose()
        $form = New-Object Windows.Forms.Form
        $form.Text = $env:FIXTURE_TITLE
        $form.StartPosition = 'CenterScreen'
        $form.ClientSize = New-Object Drawing.Size(520,365)
        $form.BackColor = [Drawing.Color]::Ivory
        $form.TopMost = $true
        $code = New-Object Windows.Forms.Label
        $code.Text = 'AMBER-7419'
        $code.Font = New-Object Drawing.Font('Arial',30)
        $code.TextAlign = 'MiddleCenter'
        $code.SetBounds(10,14,500,93)
        $form.Controls.Add($code)
        $entry = New-Object Windows.Forms.TextBox
        $entry.SetBounds(55,113,410,26)
        $entry.Font = New-Object Drawing.Font('Arial',14)
        $form.Controls.Add($entry)
        $button = New-Object Windows.Forms.Button
        $button.Text = 'Confirm test'
        $button.SetBounds(55,152,410,56)
        $button.Add_Click({ [IO.File]::WriteAllText($env:FIXTURE_CLICK_PROOF, $entry.Text); $button.Text = 'Confirmed' })
        $form.Controls.Add($button)
        $panel = New-Object Windows.Forms.Panel
        $panel.SetBounds(55,221,410,128)
        $panel.AutoScroll = $true
        $panel.BackColor = [Drawing.Color]::LightBlue
        $panel.AutoScrollMinSize = New-Object Drawing.Size(0,600)
        $scrollLabel = New-Object Windows.Forms.Label
        $scrollLabel.Text = 'Scroll here'
        $scrollLabel.SetBounds(8,8,260,32)
        $panel.Controls.Add($scrollLabel)
        $panel.Add_MouseEnter({ $panel.Focus() })
        $panel.Add_Scroll({ [IO.File]::WriteAllText($env:FIXTURE_SCROLL_PROOF, 'scrolled') })
        $panel.Add_MouseWheel({ [IO.File]::WriteAllText($env:FIXTURE_SCROLL_PROOF, 'scrolled') })
        $form.Controls.Add($panel)
        $form.Add_Shown({
            $bounds = $form.Bounds
            function Normalized($control, $x, $y) {
                $point = $control.PointToScreen([Drawing.Point]::new($x, $y))
                @([int](1000 * ($point.X - $bounds.Left) / ($bounds.Width - 1)), [int](1000 * ($point.Y - $bounds.Top) / ($bounds.Height - 1)))
            }
            $geometry = @{ entry = (Normalized $entry 180 13); button = (Normalized $button 180 28); panel = (Normalized $panel 180 75) }
            [IO.File]::WriteAllText($env:FIXTURE_GEOMETRY, ($geometry | ConvertTo-Json -Compress))
        })
        [Windows.Forms.Application]::Run($form)
    "#;
    let _fixture = Fixture(
        windows_fixture(script)
            .env("FIXTURE_TITLE", &title)
            .env("FIXTURE_CLICK_PROOF", &click_proof)
            .env("FIXTURE_SCROLL_PROOF", &scroll_proof)
            .env("FIXTURE_INPUT_IMAGE", &input_image)
            .env("FIXTURE_GEOMETRY", &geometry_file)
            .spawn()?,
    );
    let deadline = Instant::now() + Duration::from_secs(12);
    let target = loop {
        if geometry_file.is_file()
            && let Some(target) = desktop_tools::windows()
                .into_iter()
                .find(|window| window.title == title)
        {
            break target;
        }
        anyhow::ensure!(Instant::now() < deadline, "Synthetic window did not open");
        thread::sleep(Duration::from_millis(100));
    };
    let geometry: Value = serde_json::from_str(&fs::read_to_string(&geometry_file)?)?;

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

    let session = Session::new(temp.path().join("profiles"), agent.clone(), None)?;
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace)?;
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
    session.send.send(Command::SetDesktopTarget(Some(target)))?;
    let incoming: Message = connection.call(&operator, Action::Send { draft: Draft {
 mode: None,
        id: Uuid::new_v4(),
        room,
        text: format!("Use the ChatOid window tools. Capture the selected synthetic test window with share_in_chat true and read the large printed code. The exact normalized [x,y] coordinates for this fixture are {geometry}. Click 'entry', type exactly 'sınama 42', click 'button', and scroll down two steps at 'panel'. Respond with only the printed code. This is a GUI test; use the window tools, not shell commands."),
        reply: None,
        invitation: None,
        attachment: None,
    } })?;
    let deadline = Instant::now() + Duration::from_secs(105);
    let mut job_approved = false;
    let mut approved_actions = BTreeMap::new();
    let mut reported = BTreeSet::new();
    let message = loop {
        let view = session
            .receive
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
        if let Some(progress) = &view.local.workspace_progress {
            reported.insert(progress.message.clone());
        }
        let Some(job) = view
            .local
            .jobs
            .iter()
            .find(|job| job.source == incoming.seq)
        else {
            continue;
        };
        if job.status == WorkspaceJobStatus::Pending && !job_approved {
            assert!(!click_proof.exists() && !scroll_proof.exists());
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
            job_approved = true;
        }
        if let Some(approval) = &view.local.task_approval
            && approved_actions
                .insert(approval.id, approval.action.clone())
                .is_none()
        {
            assert_eq!(approval.title, title);
            session
                .send
                .send(Command::DecideTaskAction(approval.id, true))?;
        }
        if let WorkspaceJobStatus::Failed(error) = &job.status {
            anyhow::bail!("{error}");
        }
        if let Some(message) = view.local.hosts[&running.address.pin]
            .messages
            .values()
            .find(|message| message.client_id == job.id)
        {
            assert!(view.local.task_approval.is_none());
            break message.clone();
        }
    };
    assert!(
        job_approved && approved_actions.len() >= 4,
        "{} actions, {}",
        approved_actions.len(),
        message.text
    );
    assert_eq!(message.author.data.key, agent.id());
    assert_eq!(
        message.reply.as_ref().expect("quoted result").id,
        incoming.seq
    );
    assert!(message.text.contains("AMBER-7419"), "{}", message.text);
    assert_eq!(
        std::fs::read_to_string(&click_proof).with_context(|| format!("test button was not clicked; fixture geometry: {geometry}; approvals: {approved_actions:?}; reports: {reported:?}"))?,
        "sınama 42"
    );
    assert_eq!(
        std::fs::read_to_string(&scroll_proof).context("test window was not scrolled")?,
        "scrolled"
    );
    let screenshot: Attachment = connection.call(
        &operator,
        Action::Download {
            room,
            seq: message.seq,
        },
    )?;
    let screenshot = screenshot.bytes()?;
    assert!(image::load_from_memory(&screenshot)?.width() > 400);

    drop(session);
    let profiles = temp.path().join("profiles");
    let reopen_deadline = Instant::now() + Duration::from_secs(8);
    let session = loop {
        match Session::new(profiles.clone(), agent.clone(), None) {
            Ok(session) => break session,
            Err(error) if Instant::now() >= reopen_deadline => return Err(error),
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    };
    let image = fs::read(&input_image)?;
    let draft = Draft {
 mode: None,
        id: Uuid::new_v4(), room,
        text: "Continue the previous task without opening windows. Give the code you read in the previous turn, the exact text you typed, and the code printed in the newly attached image.".into(),
        reply: Some(message.seq), invitation: None, attachment: None,
    };
    for (index, chunk) in image.chunks(FILE_CHUNK).enumerate() {
        connection.upload_chunk(&operator, draft.id, index * FILE_CHUNK, chunk)?;
    }
    let follow_up: Message = connection.call(
        &operator,
        Action::SendBinary {
            draft,
            name: "görev-ekranı.png".into(),
            bytes: image.len(),
            hash: crate::host::digest(&image),
        },
    )?;
    let deadline = Instant::now() + Duration::from_secs(70);
    let mut approved = false;
    let continued = loop {
        let view = session
            .receive
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
        assert!(
            view.local.task_approval.is_none(),
            "window selection leaked into follow-up"
        );
        let Some(job) = view
            .local
            .jobs
            .iter()
            .find(|job| job.source == follow_up.seq)
        else {
            continue;
        };
        if job.status == WorkspaceJobStatus::Pending && !approved {
            assert!(
                job.continuation.is_some(),
                "previous thread was not associated with its reply"
            );
            assert_eq!(job.attachment.as_deref(), Some("görev-ekranı.png"));
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
            approved = true;
        }
        if let WorkspaceJobStatus::Failed(error) = &job.status {
            anyhow::bail!("{error}");
        }
        if let Some(reply) = view.local.hosts[&running.address.pin]
            .messages
            .values()
            .find(|reply| reply.client_id == job.id)
        {
            assert_eq!(
                view.local.workspace_threads[&job.id].thread_id,
                view.local.workspace_threads[&message.client_id].thread_id
            );
            break reply.clone();
        }
    };
    assert!(approved);
    assert_eq!(
        continued.reply.as_ref().expect("quoted follow-up").id,
        follow_up.seq
    );
    assert!(
        continued.text.contains("AMBER-7419")
            && continued.text.contains("sınama 42")
            && continued.text.contains("IMAGE-8820"),
        "{}",
        continued.text
    );
    assert!(continued.attachment.is_none());
    Ok(())
}

#[test]
#[ignore = "requires the interactive Windows desktop; no Codex login needed"]
fn approved_pointer_and_keyboard_actions_reach_only_the_selected_window() -> Result<()> {
    use crate::agent::{app_server::TaskResult, desktop_tools, task_tools::TaskTools};
    use serde_json::json;
    use std::sync::atomic::AtomicBool;

    let root = tempfile::tempdir()?;
    let events = root.path().join("events.txt");
    let ready = root.path().join("ready");
    let title = format!("Window fixture input {}", Uuid::new_v4());
    let _fixture = Fixture(windows_fixture(r#"
        Add-Type -AssemblyName System.Windows.Forms
        Add-Type -AssemblyName System.Drawing
        $form = New-Object Windows.Forms.Form
        $form.Text = $env:FIXTURE_TITLE
        $form.StartPosition = 'CenterScreen'
        $form.ClientSize = New-Object Drawing.Size(420,250)
        $form.TopMost = $true
        $entry = New-Object Windows.Forms.TextBox
        $entry.SetBounds(20,20,380,32)
        $entry.Text = 'fixture text'
        $entry.Add_KeyUp({
            if ($_.KeyCode -eq 'Home') { [IO.File]::AppendAllText($env:FIXTURE_EVENTS, 'home' + [Environment]::NewLine) }
            if ($_.KeyCode -eq 'F' -and $_.Control) { [IO.File]::AppendAllText($env:FIXTURE_EVENTS, 'find' + [Environment]::NewLine) }
        })
        $form.Controls.Add($entry)
        $surface = New-Object Windows.Forms.Panel
        $surface.SetBounds(20,65,380,165)
        $surface.BackColor = [Drawing.Color]::LightBlue
        $surface.Add_MouseClick({ if ($_.Button -eq 'Right') { [IO.File]::AppendAllText($env:FIXTURE_EVENTS, 'right' + [Environment]::NewLine) } })
        $surface.Add_MouseDoubleClick({ if ($_.Button -eq 'Left') { [IO.File]::AppendAllText($env:FIXTURE_EVENTS, 'double' + [Environment]::NewLine) } })
        $script:hoverRecorded = $false
        $script:dragStart = $null
        $surface.Add_MouseMove({
            if ($_.Button -eq 'None' -and $_.X -ge 30 -and $_.X -le 75 -and -not $script:hoverRecorded) {
                $script:hoverRecorded = $true
                [IO.File]::AppendAllText($env:FIXTURE_EVENTS, 'hover' + [Environment]::NewLine)
            }
        })
        $surface.Add_MouseDown({ if ($_.Button -eq 'Left') { $script:dragStart = $_.X } })
        $surface.Add_MouseUp({
            if ($_.Button -eq 'Left') {
                if ($null -ne $script:dragStart -and $script:dragStart -lt 170 -and $_.X -gt 250) {
                    [IO.File]::AppendAllText($env:FIXTURE_EVENTS, 'drag' + [Environment]::NewLine)
                }
                $script:dragStart = $null
            }
        })
        $form.Controls.Add($surface)
        $form.Add_Shown({ $form.Activate(); $entry.Focus(); [IO.File]::WriteAllText($env:FIXTURE_READY, 'ready') })
        [Windows.Forms.Application]::Run($form)
    "#).env("FIXTURE_TITLE", &title).env("FIXTURE_EVENTS", &events).env("FIXTURE_READY", &ready).spawn()?);
    let deadline = Instant::now() + Duration::from_secs(8);
    let target = loop {
        if ready.is_file()
            && let Some(target) = desktop_tools::windows()
                .into_iter()
                .find(|target| target.title == title)
        {
            break target;
        }
        ensure!(Instant::now() < deadline, "Input test window did not open");
        thread::sleep(Duration::from_millis(50));
    };
    let tools = TaskTools::new(root.path().to_owned(), 0, Some(target));
    let invalid = json!({"tool":"chatoid_window_drag","arguments":{"from_x":300,"from_y":620,"to_x":1001,"to_y":750}});
    assert!(
        tools
            .call(
                &invalid,
                &mut TaskResult::default(),
                &AtomicBool::new(false)
            )
            .is_err()
    );
    assert!(tools.pending().is_none());
    for (params, label, accepted, event) in [
        (
            json!({"tool":"chatoid_window_image","arguments":{"share_in_chat":false}}),
            "Pencereyi yakala",
            true,
            None,
        ),
        (
            json!({"tool":"chatoid_window_key","arguments":{"key":"Home"}}),
            "Home",
            false,
            None,
        ),
        (
            json!({"tool":"chatoid_window_key","arguments":{"key":"Home"}}),
            "Home",
            true,
            Some("home"),
        ),
        (
            json!({"tool":"chatoid_window_key","arguments":{"key":"Ctrl+F"}}),
            "Ctrl+F",
            true,
            Some("find"),
        ),
        (
            json!({"tool":"chatoid_window_click","arguments":{"action":"right","x":500,"y":620}}),
            "Sağ tık",
            true,
            Some("right"),
        ),
        (
            json!({"tool":"chatoid_window_click","arguments":{"action":"double","x":500,"y":620}}),
            "Çift tık",
            true,
            Some("double"),
        ),
        (
            json!({"tool":"chatoid_window_hover","arguments":{"x":180,"y":620}}),
            "Üzerine gel",
            true,
            Some("hover"),
        ),
        (
            json!({"tool":"chatoid_window_drag","arguments":{"from_x":300,"from_y":620,"to_x":760,"to_y":750}}),
            "Sürükle ve bırak",
            false,
            None,
        ),
        (
            json!({"tool":"chatoid_window_drag","arguments":{"from_x":300,"from_y":620,"to_x":760,"to_y":750}}),
            "Sürükle ve bırak",
            true,
            Some("drag"),
        ),
    ] {
        let original = fs::read_to_string(&events).unwrap_or_default();
        let cancelled = AtomicBool::new(false);
        let output = thread::scope(|scope| -> Result<_> {
            let action =
                scope.spawn(|| tools.call(&params, &mut TaskResult::default(), &cancelled));
            let deadline = Instant::now() + Duration::from_secs(3);
            let approval = loop {
                if let Some(approval) = tools.pending() {
                    break approval;
                }
                if action.is_finished() {
                    return action.join().expect("action thread");
                }
                if Instant::now() >= deadline {
                    cancelled.store(true, std::sync::atomic::Ordering::Release);
                    anyhow::bail!("No approval for {label}");
                }
                thread::sleep(Duration::from_millis(10));
            };
            let held = fs::read_to_string(&events).unwrap_or_default() == original;
            tools.decide(approval.id, accepted)?;
            assert!(
                held && approval.title == title && approval.action.starts_with(label),
                "Unexpected approval for {label}: {approval:?}, held={held}"
            );
            action.join().expect("action thread")
        });
        if accepted {
            let response = output?;
            assert_eq!(response["success"], true);
            if params["tool"] == "chatoid_window_image" {
                let encoded = response["contentItems"][0]["imageUrl"]
                    .as_str()
                    .and_then(|url| url.strip_prefix("data:image/jpeg;base64,"))
                    .context("No window image returned")?;
                let image = image::load_from_memory(&STANDARD.decode(encoded)?)?;
                assert!(image.width() > 300 && image.height() > 200);
            }
        } else {
            assert!(output.is_err());
        }
        if let Some(event) = event {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let recorded = fs::read_to_string(&events).unwrap_or_default();
                if recorded.lines().last() == Some(event) {
                    break;
                }
                ensure!(
                    Instant::now() < deadline,
                    "Expected input event {event}, got {recorded:?}"
                );
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let recorded = fs::read_to_string(&events).unwrap_or_default();
        if recorded
            .lines()
            .eq(["home", "find", "right", "double", "hover", "drag"])
        {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "Unexpected input events: {recorded:?}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}
