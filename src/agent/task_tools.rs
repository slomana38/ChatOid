use std::{
    io::Read,
    path::{Component, Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use cap_fs_ext::{DirExt, FollowSymlinks, MetadataExt, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use serde_json::{Value, json};
use uuid::Uuid;

use super::app_server::{TaskResult, tool_text};
use crate::protocol::check_filename;

#[cfg(target_os = "linux")]
#[path = "linux_desktop_text.rs"]
mod linux_desktop_text;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Approval {
    pub id: Uuid,
    pub title: String,
    pub action: String,
    pub file: bool,
}

pub struct ApprovedFile {
    pub name: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone)]
pub struct Question {
    pub id: Uuid,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoalStatus {
    Active,
    Complete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoalUpdate {
    pub status: GoalStatus,
    pub progress: String,
}

#[cfg(any(windows, target_os = "linux"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowRequest {
    pub id: Uuid,
    pub reason: String,
}

#[cfg(any(windows, target_os = "linux"))]
type WindowChoice = Option<super::window_tools::Target>;

#[cfg(any(windows, target_os = "linux"))]
#[derive(Default)]
pub struct DesktopRequest {
    #[cfg(windows)]
    pub reveal: bool,
    pub text_file: bool,
    pub image_before_text: bool,
}

#[cfg(any(windows, target_os = "linux"))]
pub struct DesktopDelivery {
    pub image: Vec<u8>,
    pub reply: mpsc::SyncSender<Result<()>>,
}

pub struct TaskTools {
    mcp: Mutex<Vec<super::app_server::McpConnection>>,
    root: PathBuf,
    file_limit: usize,
    pending: Mutex<Option<(Approval, mpsc::SyncSender<bool>)>>,
    question: Mutex<Option<(Question, mpsc::SyncSender<String>)>>,
    requested_desktop_capture: AtomicBool,
    goal_progress_enabled: AtomicBool,
    goal_update: Mutex<Option<GoalUpdate>>,
    #[cfg(any(windows, target_os = "linux"))]
    window: Mutex<WindowChoice>,
    #[cfg(any(windows, target_os = "linux"))]
    pub(super) window_buttons: Mutex<std::collections::BTreeMap<String, String>>,
    #[cfg(any(windows, target_os = "linux"))]
    window_request: Mutex<Option<(WindowRequest, mpsc::SyncSender<WindowChoice>)>>,
    #[cfg(any(windows, target_os = "linux"))]
    pub(super) desktop_request: DesktopRequest,
    #[cfg(any(windows, target_os = "linux"))]
    desktop_delivery: Mutex<Option<DesktopDelivery>>,
    #[cfg(any(windows, target_os = "linux"))]
    desktop_delivered: AtomicBool,
    #[cfg(any(windows, target_os = "linux"))]
    pub(super) desktop_file: Mutex<Option<String>>,
}

impl TaskTools {
    pub fn new(
        root: PathBuf,
        file_limit: usize,
        #[cfg(any(windows, target_os = "linux"))] window: Option<super::window_tools::Target>,
    ) -> Self {
        Self {
            mcp: Mutex::new(Vec::new()),
            root,
            file_limit,
            pending: Mutex::new(None),
            question: Mutex::new(None),
            requested_desktop_capture: AtomicBool::new(false),
            goal_progress_enabled: AtomicBool::new(false),
            goal_update: Mutex::new(None),
            #[cfg(any(windows, target_os = "linux"))]
            window: Mutex::new(window),
            #[cfg(any(windows, target_os = "linux"))]
            window_buttons: Mutex::new(std::collections::BTreeMap::new()),
            #[cfg(any(windows, target_os = "linux"))]
            window_request: Mutex::new(None),
            #[cfg(any(windows, target_os = "linux"))]
            desktop_request: DesktopRequest::default(),
            #[cfg(any(windows, target_os = "linux"))]
            desktop_delivery: Mutex::new(None),
            #[cfg(any(windows, target_os = "linux"))]
            desktop_delivered: AtomicBool::new(false),
            #[cfg(any(windows, target_os = "linux"))]
            desktop_file: Mutex::new(None),
        }
    }

    #[cfg(any(windows, target_os = "linux"))]
    pub fn configure_desktop(&mut self, request: DesktopRequest, delivered: bool) {
        self.desktop_request = request;
        self.desktop_delivered.store(delivered, Ordering::Release);
    }

    #[cfg(any(windows, target_os = "linux"))]
    pub fn desktop_delivered(&self) -> bool {
        self.desktop_delivered.load(Ordering::Acquire)
    }

    #[cfg(any(windows, target_os = "linux"))]
    pub fn desktop_file_written(&self) -> bool {
        self.desktop_file
            .lock()
            .expect("desktop file lock")
            .is_some()
    }

    #[cfg(any(windows, target_os = "linux"))]
    pub fn take_desktop_delivery(&self) -> Option<DesktopDelivery> {
        self.desktop_delivery.lock().ok()?.take()
    }

    #[cfg(any(windows, target_os = "linux"))]
    pub(super) fn deliver_desktop(&self, image: Vec<u8>, cancelled: &AtomicBool) -> Result<()> {
        let (reply, result) = mpsc::sync_channel(1);
        *self.desktop_delivery.lock().expect("desktop delivery lock") =
            Some(DesktopDelivery { image, reply });
        let deadline = Instant::now() + Duration::from_secs(30);
        while !cancelled.load(Ordering::Acquire) && Instant::now() < deadline {
            match result.recv_timeout(Duration::from_millis(100)) {
                Ok(outcome) => {
                    outcome?;
                    self.desktop_delivered.store(true, Ordering::Release);
                    return Ok(());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        self.take_desktop_delivery();
        anyhow::bail!("Ekran görüntüsünün sohbet teslimi doğrulanamadı");
    }

    pub fn pending(&self) -> Option<Approval> {
        self.pending
            .lock()
            .ok()?
            .as_ref()
            .map(|(approval, _)| approval.clone())
    }

    pub fn enable_mcp(&self, connections: Vec<super::app_server::McpConnection>) {
        *self.mcp.lock().expect("MCP lock") = connections;
    }

    /// Allows the single full-desktop capture the operator explicitly requested.
    /// The permission is consumed by the next share-in-chat full-desktop capture.
    pub fn allow_requested_desktop_capture(&self) {
        self.requested_desktop_capture
            .store(true, Ordering::Release);
    }

    pub fn requested_desktop_capture_allowed(&self) -> bool {
        self.requested_desktop_capture.load(Ordering::Acquire)
    }

    pub fn approve_desktop_capture(
        &self,
        share: bool,
        action: String,
        cancelled: &AtomicBool,
    ) -> Result<()> {
        if share && self.requested_desktop_capture.swap(false, Ordering::AcqRel) {
            ensure!(!cancelled.load(Ordering::Acquire), "Görev durduruldu");
            return Ok(());
        }
        self.approve("Masaüstü".into(), action, false, cancelled)
    }

    pub fn enable_goal_progress(&self) {
        self.goal_progress_enabled.store(true, Ordering::Release);
    }

    pub fn goal_progress_enabled(&self) -> bool {
        self.goal_progress_enabled.load(Ordering::Acquire)
    }

    pub fn take_goal_update(&self) -> Option<GoalUpdate> {
        self.goal_update.lock().ok()?.take()
    }

    pub fn goal_update(&self) -> Option<GoalUpdate> {
        self.goal_update.lock().ok()?.clone()
    }

    fn record_goal_progress(&self, params: &Value) -> Result<Value> {
        ensure!(
            self.goal_progress_enabled(),
            "Bu görev hedef ilerlemesi kullanmıyor"
        );
        let args = &params["arguments"];
        ensure!(
            args.is_object() && args.as_object().is_some_and(|args| args.len() == 2),
            "Hedef ilerleme parametreleri geçersiz"
        );
        let status = match args["status"].as_str().context("Hedef durumu eksik")? {
            "active" => GoalStatus::Active,
            "complete" => GoalStatus::Complete,
            _ => anyhow::bail!("Hedef durumu active veya complete olmalı"),
        };
        let progress = args["progress"]
            .as_str()
            .context("Hedef ilerlemesi eksik")?
            .trim();
        ensure!(
            !progress.is_empty() && progress.chars().count() <= 800,
            "Hedef ilerlemesi 1–800 karakter olmalı"
        );
        *self.goal_update.lock().expect("goal update lock") = Some(GoalUpdate {
            status,
            progress: progress.to_owned(),
        });
        Ok(tool_text(true, "Hedef ilerlemesi kaydedildi."))
    }

    pub(super) fn mcp_connections(&self) -> Vec<super::app_server::McpConnection> {
        self.mcp.lock().expect("MCP lock").clone()
    }

    pub(super) fn approve_mcp(&self, params: &Value, cancelled: &AtomicBool) -> Result<()> {
        ensure!(
            params["mode"] == "form" && params["_meta"]["codex_approval_kind"] == "mcp_tool_call",
            "Bu MCP formu desteklenmiyor"
        );
        let name = params["serverName"]
            .as_str()
            .context("MCP sunucusu eksik")?;
        let tool = params["_meta"]["tool_name"]
            .as_str()
            .context("MCP aracı eksik")?;
        let connections = self.mcp_connections();
        ensure!(
            connections.iter().any(|connection| connection.name == name
                && connection.tools.values().any(|entry| entry.name == tool)),
            "MCP aracı bu görevde seçilmedi"
        );
        let args = &params["_meta"]["tool_params"];
        ensure!(args.is_object(), "MCP parametreleri eksik");
        let args = serde_json::to_string_pretty(args)?;
        ensure!(args.len() <= 64 * 1024, "MCP parametreleri çok büyük");
        self.approve(
            format!("{name} / {tool}"),
            format!("MCP araç çağrısı\n{args}"),
            false,
            cancelled,
        )
    }

    #[cfg(any(windows, target_os = "linux"))]
    pub fn window(&self) -> WindowChoice {
        self.window.lock().ok()?.clone()
    }

    #[cfg(any(windows, target_os = "linux"))]
    pub fn window_request(&self) -> Option<WindowRequest> {
        self.window_request
            .lock()
            .ok()?
            .as_ref()
            .map(|(request, _)| request.clone())
    }

    #[cfg(any(windows, target_os = "linux"))]
    pub fn choose_window(&self, id: Uuid, target: WindowChoice) -> Result<()> {
        let mut pending = self.window_request.lock().expect("window request lock");
        ensure!(
            pending
                .as_ref()
                .is_some_and(|(request, _)| request.id == id),
            "Pencere isteğinin süresi doldu"
        );
        if let Some(target) = &target {
            target.checked()?;
        }
        let (_, sender) = pending.take().expect("checked window request");
        sender
            .try_send(target)
            .context("Pencere isteğinin süresi doldu")
    }

    #[cfg(any(windows, target_os = "linux"))]
    pub(super) fn request_window(&self, reason: String, cancelled: &AtomicBool) -> Result<Value> {
        let (sender, receiver) = mpsc::sync_channel(1);
        let id = Uuid::new_v4();
        {
            let mut pending = self.window_request.lock().expect("window request lock");
            ensure!(pending.is_none(), "Başka bir pencere isteği bekliyor");
            *pending = Some((WindowRequest { id, reason }, sender));
        }
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut target = None;
        while !cancelled.load(Ordering::Acquire) && Instant::now() < deadline {
            match receiver.recv_timeout(Duration::from_millis(120)) {
                Ok(choice) => {
                    target = choice;
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let mut pending = self.window_request.lock().expect("window request lock");
        pending.take_if(|(request, _)| request.id == id);
        drop(pending);
        ensure!(!cancelled.load(Ordering::Acquire), "Görev durduruldu");
        let Some(target) = target else {
            return Ok(tool_text(false, "Operatör yeni bir pencere seçmedi."));
        };
        target.checked()?;
        let result = format!(
            "Seçilen pencere: {}. Yeni koordinatları belirlemek için bu pencerenin görüntüsünü al. Her eylem ayrı onay ister.",
            target.title
        );
        *self.window.lock().expect("window target lock") = Some(target);
        self.window_buttons
            .lock()
            .expect("window button lock")
            .clear();
        Ok(tool_text(true, &result))
    }

    pub fn question(&self) -> Option<Question> {
        self.question
            .lock()
            .ok()?
            .as_ref()
            .map(|(question, _)| question.clone())
    }

    pub fn answer(&self, id: Uuid, text: String) -> Result<()> {
        let mut pending = self.question.lock().expect("question lock");
        ensure!(
            pending
                .as_ref()
                .is_some_and(|(question, _)| question.id == id),
            "Görev artık yanıt beklemiyor"
        );
        let (_, sender) = pending.take().expect("checked question");
        sender.try_send(text).context("Sorunun süresi doldu")
    }

    fn ask(&self, params: &Value, cancelled: &AtomicBool) -> Result<Value> {
        let text = params["arguments"]["question"]
            .as_str()
            .context("Soru metni eksik")?
            .trim();
        ensure!(
            !text.is_empty() && text.chars().count() <= 750,
            "Soru 1–750 karakter olmalı"
        );
        let deadline = Instant::now() + Duration::from_secs(180);
        Ok(match self.wait_for_answer(text, deadline, cancelled)? {
            Some(answer) => tool_text(true, &answer),
            None => tool_text(false, "Operatör süresi içinde cevap vermedi."),
        })
    }

    pub(super) fn plan_answers(&self, params: &Value, cancelled: &AtomicBool) -> Result<Value> {
        let mut answers = serde_json::Map::new();
        let deadline = Instant::now() + Duration::from_secs(180);
        for question in params["questions"].as_array().into_iter().flatten().take(3) {
            if question["isSecret"] == true || Instant::now() >= deadline {
                continue;
            }
            let (Some(id), Some(text)) = (question["id"].as_str(), question["question"].as_str())
            else {
                continue;
            };
            if text.trim().is_empty() || answers.contains_key(id) {
                continue;
            }
            let mut prompt: String = text.trim().chars().take(420).collect();
            for option in question["options"].as_array().into_iter().flatten().take(3) {
                if let Some(label) = option["label"].as_str() {
                    prompt.push_str("\n• ");
                    prompt.extend(label.chars().take(70));
                }
            }
            if let Some(answer) = self.wait_for_answer(&prompt, deadline, cancelled)? {
                answers.insert(id.to_owned(), json!({"answers":[answer]}));
            }
        }
        Ok(Value::Object(answers))
    }

    fn wait_for_answer(
        &self,
        text: &str,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<Option<String>> {
        let (sender, receiver) = mpsc::sync_channel(1);
        let id = Uuid::new_v4();
        {
            let mut pending = self.question.lock().expect("question lock");
            ensure!(pending.is_none(), "Başka bir görev sorusu bekliyor");
            *pending = Some((
                Question {
                    id,
                    text: text.into(),
                },
                sender,
            ));
        }
        let mut result = None;
        while !cancelled.load(Ordering::Acquire) && Instant::now() < deadline {
            match receiver.recv_timeout(Duration::from_millis(120)) {
                Ok(answer) => {
                    result = Some(answer);
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        self.question
            .lock()
            .expect("question lock")
            .take_if(|(question, _)| question.id == id);
        ensure!(!cancelled.load(Ordering::Acquire), "Görev durduruldu");
        Ok(result)
    }

    pub fn decide(&self, id: Uuid, accept: bool) -> Result<()> {
        let mut pending = self.pending.lock().expect("approval lock");
        ensure!(
            pending
                .as_ref()
                .is_some_and(|(approval, _)| approval.id == id),
            "Görev eylemi artık beklemiyor"
        );
        let (_, sender) = pending.take().expect("checked approval");
        sender
            .try_send(accept)
            .context("Görev eyleminin süresi doldu")
    }

    pub(super) fn approve(
        &self,
        title: String,
        action: String,
        file: bool,
        cancelled: &AtomicBool,
    ) -> Result<()> {
        let id = Uuid::new_v4();
        let approval = Approval {
            id,
            title,
            action,
            file,
        };
        let (send, receive) = mpsc::sync_channel(1);
        {
            let mut pending = self.pending.lock().expect("approval lock");
            ensure!(pending.is_none(), "Başka bir görev eylemi onay bekliyor");
            *pending = Some((approval, send));
        }
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut allowed = false;
        while !cancelled.load(Ordering::Acquire) && Instant::now() < deadline {
            match receive.recv_timeout(Duration::from_millis(120)) {
                Ok(accept) => {
                    allowed = accept;
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        self.pending
            .lock()
            .expect("approval lock")
            .take_if(|(approval, _)| approval.id == id);
        ensure!(
            allowed && !cancelled.load(Ordering::Acquire) && Instant::now() < deadline,
            "Görev eylemi onaylanmadı"
        );
        Ok(())
    }

    pub fn call(
        &self,
        params: &Value,
        result: &mut TaskResult,
        cancelled: &AtomicBool,
    ) -> Result<Value> {
        if params["tool"] == "chatoid_goal_progress" {
            return self.record_goal_progress(params);
        }
        if params["tool"] == "chatoid_ask_user" {
            return self.ask(params, cancelled);
        }
        #[cfg(target_os = "linux")]
        if params["tool"] == "chatoid_desktop_text" {
            return linux_desktop_text::call(self, params, cancelled);
        }
        if params["tool"] != "chatoid_share_file" {
            #[cfg(windows)]
            return super::desktop_tools::call(self, params, result, cancelled);
            #[cfg(target_os = "linux")]
            return super::linux_desktop_tools::call(self, params, result, cancelled);
            #[cfg(not(any(windows, target_os = "linux")))]
            anyhow::bail!("Bu görev aracı kullanılabilir değil");
        }
        ensure!(
            result.file.is_none() && result.screenshot.is_none(),
            "Görev sonucu için bir ek zaten onaylandı"
        );
        let path = params["arguments"]["path"]
            .as_str()
            .context("Paylaşılacak dosyanın göreli yolu eksik")?;
        let file = snapshot(&self.root, path, self.file_limit)?;
        self.approve(
            file.name.clone(),
            format!(
                "Bu görev sohbetine gönder · {} bayt\n{path}",
                file.bytes.len()
            ),
            true,
            cancelled,
        )?;
        result.file = Some(file);
        Ok(tool_text(
            true,
            "Dosya yerelde onaylandı ve görev tamamlanınca sohbet yanıtına eklenecek.",
        ))
    }
}

pub fn definitions(tools: Option<&TaskTools>) -> Vec<Value> {
    let file = json!({"type":"function","name":"chatoid_share_file",
        "description":"Attach one file from the approved workspace to the final ChatOid reply after separate local approval. Use only when the user requested a file. Supply a path relative to the workspace and call this after finishing the file.",
        "inputSchema":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}});
    let question = json!({"type":"function","name":"chatoid_ask_user",
        "description":"Ask the operator one necessary clarification in their ChatOid conversation and wait for an answer. Do not include private workspace content in the question or ask for credentials or approval for a tool action. Use the operator's language.",
        "inputSchema":{"type":"object","properties":{"question":{"type":"string"}},"required":["question"],"additionalProperties":false}});
    let goal_progress = json!({"type":"function","name":"chatoid_goal_progress",
        "description":"Report a real update to the persistent goal explicitly tracked for this task. Use status active with concrete progress while work remains, or complete only after the goal is achieved.",
        "inputSchema":{"type":"object","properties":{"status":{"type":"string","enum":["active","complete"]},"progress":{"type":"string","minLength":1,"maxLength":800}},"required":["status","progress"],"additionalProperties":false}});
    let mut definitions = vec![file, question];
    if tools.is_some_and(TaskTools::goal_progress_enabled) {
        definitions.push(goal_progress);
    }
    #[cfg(windows)]
    definitions.extend(super::desktop_tools::definitions());
    #[cfg(target_os = "linux")]
    definitions.extend(super::linux_desktop_tools::definitions());
    #[cfg(any(windows, target_os = "linux"))]
    if tools.is_some_and(|tools| tools.desktop_request.text_file) {
        #[cfg(windows)]
        definitions.push(super::desktop_tools::desktop_text_definition());
        #[cfg(target_os = "linux")]
        definitions.push(linux_desktop_text::definition());
    }
    definitions
}

fn snapshot(root: &Path, relative: &str, limit: usize) -> Result<ApprovedFile> {
    ensure!(limit > 0, "Bu host ajan görev dosyalarını desteklemiyor");
    ensure!(relative.chars().count() <= 1024, "Geçersiz dosya yolu");
    let mut directory = Dir::open_ambient_dir(root, ambient_authority())?;
    let mut parts = Path::new(relative).components().peekable();
    while let Some(part) = parts.next() {
        let Component::Normal(name) = part else {
            anyhow::bail!("Klasör dışından dosya paylaşılamaz");
        };
        let label = name.to_str().context("Geçersiz dosya adı")?;
        check_filename(label)?;
        ensure!(!super::private_file_name(label), "Bu dosya paylaşılamaz");
        if parts.peek().is_some() {
            directory = directory.open_dir_nofollow(name)?;
            continue;
        }
        ensure!(
            directory.symlink_metadata(name)?.is_file(),
            "Normal bir dosya seç"
        );
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = directory.open_with(name, &options)?;
        let metadata = file.metadata()?;
        ensure!(metadata.nlink() == 1, "Bağlantılı dosyalar paylaşılamaz");
        ensure!(
            metadata.is_file() && (1..=limit as u64).contains(&metadata.len()),
            "Dosya boyutu host sınırını aşıyor veya dosya boş"
        );
        let mut bytes = Vec::new();
        file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 == metadata.len(),
            "Dosya okunurken değişti"
        );
        return Ok(ApprovedFile {
            name: label.to_owned(),
            bytes,
        });
    }
    anyhow::bail!("Paylaşılacak dosya bulunamadı")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn task_output_rejects_paths_and_links_outside_the_workspace() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("workspace");
        std::fs::create_dir(&root)?;
        let outside = temp.path().join("private.txt");
        std::fs::write(&outside, b"test fixture outside the workspace")?;
        std::fs::write(root.join("report.txt"), b"test report")?;
        std::fs::hard_link(&outside, root.join("linked.txt"))?;
        assert!(snapshot(&root, "../private.txt", 1024).is_err());
        assert!(snapshot(&root, outside.to_str().context("test path")?, 1024).is_err());
        assert!(snapshot(&root, "linked.txt", 1024).is_err());
        assert!(snapshot(&root, "report.txt", 1).is_err());
        assert_eq!(snapshot(&root, "report.txt", 1024)?.bytes, b"test report");
        Ok(())
    }

    #[test]
    fn task_output_rejects_local_credentials_and_database_sidecars() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        for name in [
            "owner.IDENTITY",
            "auth.json",
            "history.sqlite-wal",
            "state.db-shm",
            "host.db-journal",
        ] {
            std::fs::write(workspace.path().join(name), b"synthetic fixture")?;
            assert!(snapshot(workspace.path(), name, 1024).is_err(), "{name}");
        }
        std::fs::write(workspace.path().join("author.json"), b"report")?;
        assert_eq!(
            snapshot(workspace.path(), "author.json", 1024)?.bytes,
            b"report"
        );
        Ok(())
    }

    #[test]
    fn a_pending_action_or_question_cannot_be_replaced() -> Result<()> {
        let tools = TaskTools::new(
            PathBuf::new(),
            0,
            #[cfg(any(windows, target_os = "linux"))]
            None,
        );
        let cancelled = AtomicBool::new(false);
        let (send_approval, receive_approval) = mpsc::sync_channel(1);
        let id = Uuid::new_v4();
        *tools.pending.lock().expect("approval lock") = Some((
            Approval {
                id,
                title: "first".into(),
                action: "action".into(),
                file: false,
            },
            send_approval,
        ));
        assert!(
            tools
                .approve("second".into(), "action".into(), false, &cancelled)
                .is_err()
        );
        assert_eq!(tools.pending().map(|approval| approval.id), Some(id));
        tools.decide(id, true)?;
        assert!(receive_approval.recv()?);

        let (send_answer, receive_answer) = mpsc::sync_channel(1);
        let id = Uuid::new_v4();
        *tools.question.lock().expect("question lock") = Some((
            Question {
                id,
                text: "first".into(),
            },
            send_answer,
        ));
        assert!(
            tools
                .wait_for_answer("second", Instant::now(), &cancelled)
                .is_err()
        );
        assert_eq!(tools.question().map(|question| question.id), Some(id));
        tools.answer(id, "answer".into())?;
        assert_eq!(receive_answer.recv()?, "answer");

        #[cfg(any(windows, target_os = "linux"))]
        {
            let (send_choice, receive_choice) = mpsc::sync_channel(1);
            let id = Uuid::new_v4();
            *tools.window_request.lock().expect("window request lock") = Some((
                WindowRequest {
                    id,
                    reason: "first".into(),
                },
                send_choice,
            ));
            assert!(tools.request_window("second".into(), &cancelled).is_err());
            assert_eq!(tools.window_request().map(|request| request.id), Some(id));
            tools.choose_window(id, None)?;
            assert!(receive_choice.recv()?.is_none());
        }
        Ok(())
    }

    #[test]
    fn requested_desktop_capture_is_one_shot_and_share_only() -> Result<()> {
        let tools = TaskTools::new(
            PathBuf::new(),
            0,
            #[cfg(any(windows, target_os = "linux"))]
            None,
        );
        let cancelled = Arc::new(AtomicBool::new(false));
        tools.allow_requested_desktop_capture();
        let worker_tools = Arc::new(tools);
        let ui_tools = worker_tools.clone();
        let pending_tools = worker_tools.clone();
        let worker_cancelled = cancelled.clone();
        let worker = std::thread::spawn(move || {
            pending_tools.approve_desktop_capture(
                false,
                "planning capture".into(),
                &worker_cancelled,
            )
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while ui_tools.pending().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let approval = ui_tools
            .pending()
            .context("share=false should still request approval")?;
        ui_tools.decide(approval.id, true)?;
        worker
            .join()
            .map_err(|_| anyhow::anyhow!("worker panicked"))??;
        worker_tools.approve_desktop_capture(
            true,
            "explicitly requested capture".into(),
            &cancelled,
        )?;
        assert!(!worker_tools.requested_desktop_capture_allowed());

        let worker_tools = worker_tools.clone();
        let worker_cancelled = cancelled.clone();
        let worker = std::thread::spawn(move || {
            worker_tools.approve_desktop_capture(true, "second capture".into(), &worker_cancelled)
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while ui_tools.pending().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let approval = ui_tools
            .pending()
            .context("second capture should require approval")?;
        ui_tools.decide(approval.id, false)?;
        assert!(
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("worker panicked"))?
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn goal_progress_tool_requires_enablement_and_valid_values() -> Result<()> {
        let tools = TaskTools::new(
            PathBuf::new(),
            0,
            #[cfg(any(windows, target_os = "linux"))]
            None,
        );
        let mut result = TaskResult::default();
        let cancelled = AtomicBool::new(false);
        let call = |status: &str, progress: &str| json!({"tool":"chatoid_goal_progress","arguments":{"status":status,"progress":progress}});
        assert!(
            tools
                .call(&call("active", "started"), &mut result, &cancelled)
                .is_err()
        );
        assert!(tools.take_goal_update().is_none());
        tools.enable_goal_progress();
        tools.call(
            &call("active", "Implementation is underway."),
            &mut result,
            &cancelled,
        )?;
        assert_eq!(
            tools.take_goal_update(),
            Some(GoalUpdate {
                status: GoalStatus::Active,
                progress: "Implementation is underway.".into(),
            })
        );
        assert!(
            tools
                .call(&call("unknown", "progress"), &mut result, &cancelled)
                .is_err()
        );
        assert!(
            tools
                .call(&call("complete", &"x".repeat(801)), &mut result, &cancelled)
                .is_err()
        );
        assert!(tools.take_goal_update().is_none());
        Ok(())
    }
}
