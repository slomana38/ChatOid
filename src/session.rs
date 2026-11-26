use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::Read,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection as Database, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    agent::{self, Settings},
    identity::{Card, Identity, Role, Signed, encode, pack, unpack, write_new},
    local_file::{open_regular, read_bounded},
    protocol::*,
    transport::{self, Connection, RemoteError, RunningHost},
};

#[cfg(any(windows, target_os = "linux"))]
const DESKTOP_SCREENSHOT_NAME: &str = "Ekran görüntüsü.jpg";
const TASK_QUESTION: &str = "Codex görev sorusu\n";

pub mod automation;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub host: String,
    pub room: Uuid,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Pending {
    pub draft: Draft,
    pub error: Option<String>,
    #[serde(default)]
    pub binary: Option<PathBuf>,
    #[serde(default)]
    task_question: bool,
}

#[derive(Default)]
struct Transfers {
    progress: BTreeMap<Uuid, (usize, usize)>,
    cancelled: BTreeSet<Uuid>,
    active: BTreeSet<Uuid>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Link {
    pub address: HostAddress,
    cursor: i64,
    pub rooms: Vec<Room>,
    pub requests: Vec<Request>,
    pub messages: BTreeMap<i64, Message>,
    pub queue: Vec<Pending>,
    pub archived: BTreeSet<Uuid>,
    pub seen: BTreeMap<Uuid, i64>,
    #[serde(default)]
    cleared_through: BTreeMap<Uuid, i64>,
    #[serde(default)]
    pub receipts: Vec<Receipt>,
    #[serde(default)]
    pub attachments: bool,
    #[serde(default)]
    pub max_file_bytes: usize,
    #[serde(default)]
    pub agent_binary_files: bool,
    #[serde(default)]
    pub message_modes: bool,
    #[serde(skip)]
    pub supports_task_activity: bool,
    #[serde(skip)]
    pub task_activity: Vec<TaskActivity>,
    #[serde(default)]
    pub avatars: BTreeMap<String, AvatarPhoto>,
    #[serde(default)]
    avatar_current: Option<String>,
    #[serde(skip)]
    pub online: bool,
    #[serde(skip)]
    pub error: String,
}

impl Link {
    fn new(address: HostAddress) -> Self {
        Self {
            address,
            cursor: 0,
            rooms: vec![],
            requests: vec![],
            messages: BTreeMap::new(),
            queue: vec![],
            archived: BTreeSet::new(),
            seen: BTreeMap::new(),
            cleared_through: BTreeMap::new(),
            receipts: vec![],
            attachments: false,
            max_file_bytes: 0,
            agent_binary_files: false,
            message_modes: false,
            supports_task_activity: false,
            task_activity: Vec::new(),
            avatars: BTreeMap::new(),
            avatar_current: None,
            online: false,
            error: String::new(),
        }
    }
    pub fn cleared_through(&self, room: Uuid) -> i64 {
        self.cleared_through.get(&room).copied().unwrap_or_default()
    }
    pub fn is_visible(&self, room: Uuid, seq: i64) -> bool {
        seq > self.cleared_through(room)
    }
    pub fn visible_messages(&self, room: Uuid) -> impl DoubleEndedIterator<Item = &Message> {
        self.messages
            .range((self.cleared_through(room) + 1)..)
            .filter_map(move |(_, message)| (message.room == room).then_some(message))
    }
    pub fn unread_messages(&self, room: Uuid, viewer: &str) -> impl Iterator<Item = &Message> {
        let after = self
            .seen
            .get(&room)
            .copied()
            .unwrap_or_default()
            .max(self.cleared_through(room));
        self.messages
            .range((after + 1)..)
            .filter_map(move |(_, m)| (m.room == room && m.author.data.key != viewer).then_some(m))
    }
    pub fn unread(&self, room: Uuid, viewer: &str) -> usize {
        self.unread_messages(room, viewer).count()
    }
    pub fn last(&self, room: Uuid) -> Option<&Message> {
        self.visible_messages(room).next_back()
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Local {
    pub loops: Vec<automation::Loop>,
    pub goals: Vec<automation::Goal>,
    pub hosts: BTreeMap<String, Link>,
    pub held_messages: BTreeMap<String, Message>,
    pub contacts: BTreeMap<String, Card>,
    pub grants: Vec<Grant>,
    pub operator: Option<Card>,
    pub tickets: BTreeMap<Uuid, Ticket>,
    pub bind: Option<SocketAddr>,
    pub settings: Settings,
    pub jobs: Vec<WorkspaceJob>,
    workspace_threads: BTreeMap<Uuid, WorkspaceContinuation>,
    #[serde(skip)]
    pub workspace_progress: Option<agent::app_server::TaskProgress>,
    #[serde(default)]
    pub workspace_reports: BTreeMap<Uuid, Arc<agent::app_server::TaskProgress>>,
    #[serde(skip)]
    pub task_models: Vec<agent::app_server::CatalogModel>,
    #[serde(skip)]
    pub task_skills: Option<Vec<agent::app_server::TaskSkill>>,
    #[serde(skip)]
    pub task_mcp: Option<Vec<agent::app_server::McpConnection>>,
    #[cfg(any(windows, target_os = "linux"))]
    #[serde(skip)]
    pub desktop_target: Option<agent::window_tools::Target>,
    #[cfg(any(windows, target_os = "linux"))]
    #[serde(skip)]
    pub desktop_active: Option<agent::window_tools::Target>,
    #[cfg(any(windows, target_os = "linux"))]
    #[serde(skip)]
    pub desktop_request: Option<agent::task_tools::WindowRequest>,
    #[serde(skip)]
    pub task_approval: Option<agent::task_tools::Approval>,
    pub avatar: Option<AvatarUpdate>,
    pub display_name: Option<String>,
    pub share_device: Option<bool>,
    pub profiles: BTreeMap<String, ProfilePublication>,
    pub removals: Vec<Selection>,
    outbound_introductions: Vec<(Signed<ContactRoute>, Ticket)>,
    incoming_introductions: BTreeMap<Uuid, Ticket>,
    pending_contacts: Vec<Signed<ContactRoute>>,
    pending_contact_tickets: BTreeMap<String, (Ticket, Signed<Terms>)>,
    pending_group_offers: Vec<(Selection, Card)>,
    pending_group_decisions: Vec<(String, Request)>,
    completed: BTreeMap<String, Option<String>>,
    pub opened_invites: BTreeSet<Uuid>,
}

impl Local {
    pub fn friend(&self, me: &str, other: &str) -> Option<Friendship> {
        self.hosts
            .values()
            .flat_map(|host| {
                host.requests
                    .iter()
                    .filter(|request| self.friendship_open(host, request))
            })
            .filter_map(Request::friendship)
            .find(|proof| proof.check(me, other).is_ok())
    }
    pub fn already_requested(&self, me: &str, other: &str) -> bool {
        self.hosts.values().any(|host| {
            host.requests.iter().any(|r| {
                let t = &r.terms.data;
                t.room.is_none()
                    && (r.status == Status::Pending || self.friendship_open(host, r))
                    && ((t.from == me && t.to == other) || (t.from == other && t.to == me))
            })
        })
    }

    pub fn friendship_open(&self, host: &Link, request: &Request) -> bool {
        let room = request.terms.data.id;
        request.status == Status::Accepted
            && request.terms.data.room.is_none()
            && host.rooms.iter().any(|r| r.id == room && !r.closed)
            && !self
                .removals
                .iter()
                .any(|r| r.room == room && r.host == host.address.pin)
    }

    pub fn introduction_waiting(&self, id: Uuid) -> bool {
        self.outbound_introductions
            .iter()
            .any(|(_, ticket)| ticket.id == id)
    }

    pub fn group_decision_waiting(&self, id: Uuid) -> bool {
        self.pending_group_decisions
            .iter()
            .any(|(_, request)| request.terms.data.id == id)
    }
}

fn prepare_avatar(path: &std::path::Path) -> Result<String> {
    let file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= 12 * 1024 * 1024,
        "En fazla 12 MiB boyutunda JPEG veya PNG seç"
    );
    let mut bytes = Vec::new();
    file.take(12 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 12 * 1024 * 1024,
        "Profil fotoğrafı boyutu aşıldı"
    );
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    ensure!(
        matches!(
            reader.format(),
            Some(image::ImageFormat::Jpeg | image::ImageFormat::Png)
        ),
        "JPEG veya PNG seç"
    );
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(80 * 1024 * 1024);
    reader.limits(limits);
    let thumbnail = reader
        .decode()?
        .resize_to_fill(AVATAR_PX, AVATAR_PX, image::imageops::FilterType::Triangle)
        .to_rgb8();
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 84).encode_image(&thumbnail)?;
    let data = encode(jpeg);
    AvatarPhoto::decode(&data)?;
    Ok(data)
}

pub fn device_summary() -> DeviceSummary {
    let cores = thread::available_parallelism().map_or(0, |count| count.get());
    #[cfg(windows)]
    let (system, memory) = {
        use std::{os::windows::process::CommandExt, process::Command};
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut memory = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        // The Windows API fills a caller-owned structure with aggregate memory values.
        let ram = if unsafe { GlobalMemoryStatusEx(&mut memory) } != 0 {
            let gib = memory.ullTotalPhys.div_ceil(1024_u64.pow(3));
            format!("{gib} GiB sınıfı")
        } else {
            "Belirtilmedi".into()
        };
        let build = Command::new("reg.exe")
            .args([
                "query",
                r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion",
                "/v",
                "CurrentBuildNumber",
            ])
            .creation_flags(0x08000000)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .and_then(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .last()?
                    .parse::<u32>()
                    .ok()
            });
        let system = build.map_or_else(
            || "Windows".into(),
            |build| {
                format!(
                    "Windows {} · derleme {build}",
                    if build >= 22000 { "11" } else { "10" }
                )
            },
        );
        (system, ram)
    };
    #[cfg(not(windows))]
    let (system, memory) = (std::env::consts::OS.into(), "Belirtilmedi".into());
    DeviceSummary {
        system,
        memory,
        architecture: std::env::consts::ARCH.into(),
        processor: if cores == 0 {
            "Belirtilmedi".into()
        } else {
            format!("{cores} mantıksal işlemci sınıfı")
        },
    }
}

pub enum Command {
    LoadTaskModels,
    LoadTaskSkills,
    LoadTaskMcp,
    SignInMcp(String),
    AddTaskMcp(agent::app_server::McpSetup),
    DecideWorkspaceJob(Uuid, WorkspaceDecision),
    CancelWorkspaceJob(Uuid),
    RetryWorkspaceJob(Uuid),
    #[cfg(any(windows, target_os = "linux"))]
    SetDesktopTarget(Option<agent::window_tools::Target>),
    #[cfg(any(windows, target_os = "linux"))]
    DecideDesktopTarget(Uuid, Option<agent::window_tools::Target>),
    DecideTaskAction(Uuid, bool),
    Start(SocketAddr),
    Stop,
    Import(String),
    Pair(String),
    Offer(Card),
    OfferContact(Signed<ContactRoute>),
    ShareContact,
    EndFriendship(Selection),
    Group(String),
    Invite(Selection, Card),
    Decide(String, Box<Request>, bool),
    SendDraft(Selection, Uuid, String, Option<i64>, Option<PathBuf>),
    SendModeDraft(
        Selection,
        Uuid,
        String,
        Option<i64>,
        Option<PathBuf>,
        Option<MessageMode>,
    ),
    WithdrawMessage(Selection, i64),
    Download(Selection, i64, PathBuf),
    SetAvatar(Option<PathBuf>),
    SetBuiltInAvatar(usize),
    SetDisplayName(String),
    SetDeviceSharing(bool),
    LookupProfile(String),
    SetDiscovery(bool),
    History(Selection, i64),
    ClearChat(Selection),
    Archive(Selection, bool),
    Read(Selection, i64),
    Leave(Selection),
    Remove(Selection, String),
    Close(Selection),
    Retry(Selection, Uuid),
    CancelSend(Selection, Uuid),
    Settings(Settings),
    SignInCodex(String),
    SignInTaskCodex(String),
    CheckCodex,
    RetryCodex,
}

pub struct View {
    pub local: Local,
    pub host: Option<HostAddress>,
    pub notice: Option<String>,
    pub copy: Option<String>,
    pub codex: String,
}

enum ModelTask {
    McpAdded {
        codex: String,
        name: String,
        result: Result<()>,
    },
    McpSignIn {
        codex: String,
        server: String,
        result: Result<Vec<agent::app_server::McpConnection>>,
    },
    McpCatalog {
        codex: String,
        result: Result<Vec<agent::app_server::McpConnection>>,
    },
    Catalog {
        codex: String,
        result: Result<Vec<agent::app_server::CatalogModel>>,
    },
    SkillCatalog {
        codex: String,
        directory: String,
        roots: Vec<PathBuf>,
        result: Result<Vec<agent::app_server::TaskSkill>>,
    },
    Workspace {
        id: Uuid,
        directory: PathBuf,
        result: Result<agent::app_server::TaskResult>,
    },
    SignIn {
        codex: String,
        tasks: bool,
    },
    Check,
    Reply {
        host: String,
        room: Uuid,
        source: i64,
        result: Result<Option<String>>,
    },
    Decide {
        host: String,
        request: Box<Request>,
        result: Result<Option<bool>>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceJob {
    #[serde(default)]
    pub automation: Option<AutomationRef>,
    pub id: Uuid,
    pub selection: Selection,
    pub source: i64,
    pub actor: String,
    pub task: String,
    pub directory: PathBuf,
    #[serde(default)]
    pub continuation: Option<WorkspaceContinuation>,
    #[serde(default)]
    pub recovery: Option<WorkspaceContinuation>,
    #[serde(default)]
    pub approval_round: u32,
    #[serde(default)]
    pub attachment: Option<String>,
    #[serde(default)]
    pub web_research: bool,
    #[serde(default)]
    pub network: bool,
    #[serde(default)]
    pub delegate: bool,
    #[serde(default)]
    pub model: Option<agent::app_server::ModelChoice>,
    #[serde(default)]
    pub plan_only: bool,
    #[serde(default)]
    pub review: Option<agent::app_server::ReviewTarget>,
    #[serde(default)]
    pub skill_names: Vec<String>,
    #[serde(default)]
    pub forked: bool,
    pub status: WorkspaceJobStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutomationRef {
    Loop(Uuid),
    Goal(Uuid),
}

impl WorkspaceJob {
    pub fn can_fork(&self) -> bool {
        self.continuation
            .as_ref()
            .is_some_and(|source| source.isolated && source.turn_id.is_some())
    }
}

pub enum WorkspaceDecision {
    Approve {
        web_research: bool,
        network: bool,
        delegate: bool,
        model: Option<agent::app_server::ModelChoice>,
        plan_only: bool,
        skills: Vec<PathBuf>,
        compact_before: bool,
        fork: bool,
        mcp: bool,
    },
    Review {
        model: Option<agent::app_server::ModelChoice>,
        target: agent::app_server::ReviewTarget,
    },
    Reject,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceContinuation {
    thread_id: String,
    #[serde(default)]
    turn_id: Option<String>,
    #[serde(default)]
    isolated: bool,
    directory: PathBuf,
    selection: Selection,
    actor: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkspaceJobStatus {
    Pending,
    Running,
    Complete,
    Rejected,
    Cancelled,
    Failed(String),
}

pub struct Session {
    pub send: mpsc::Sender<Command>,
    pub receive: mpsc::Receiver<View>,
    transfers: Arc<Mutex<Transfers>>,
}

impl Session {
    pub fn new(path: PathBuf, identity: Identity, inbox: Option<PathBuf>) -> Result<Self> {
        let dir = path.join(identity.id());
        let mut directory = fs::DirBuilder::new();
        directory.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directory.mode(0o700);
        }
        directory.create(&dir)?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(".session.lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock)
            .context("Bu hesap başka bir ChatOid penceresinde açık")?;
        let inbox = inbox.filter(|root| fs::create_dir_all(root.join(identity.id())).is_ok());
        let db = Database::open(dir.join("client.sqlite3"))?;
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS client(id INTEGER PRIMARY KEY, data TEXT NOT NULL);")?;
        let local = db
            .query_row("SELECT data FROM client WHERE id=1", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or_default();
        let (send, input) = mpsc::channel();
        let (output, receive) = mpsc::channel();
        let transfers = Arc::new(Mutex::new(Transfers::default()));
        let worker_transfers = transfers.clone();
        thread::Builder::new()
            .name("chatoid-client".into())
            .spawn(move || {
                let _lock = lock;
                Worker {
                    db,
                    dir,
                    inbox,
                    identity,
                    local,
                    connections: HashMap::new(),
                    running: None,
                    notice: None,
                    copy: None,
                    codex: "Codex bekliyor".into(),
                    model: None,
                    last_failed: None,
                    retry_after: HashMap::new(),
                    published: BTreeMap::new(),
                    device: device_summary(),
                    transfers: worker_transfers,
                    upload_job: None,
                    workspace_cancel: None,
                    workspace_steering: None,
                    workspace_progress: None,
                    task_activity_sent: BTreeMap::new(),
                    task_tools: None,
                    admitted_messages: BTreeSet::new(),
                }
                .run(input, output)
            })?;
        Ok(Self {
            send,
            receive,
            transfers,
        })
    }

    pub fn transfer_progress(&self, id: Uuid) -> Option<(usize, usize)> {
        self.transfers.lock().ok()?.progress.get(&id).copied()
    }

    pub fn cancel_transfer(&self, selection: Selection, id: Uuid) {
        if let Ok(mut transfers) = self.transfers.lock() {
            transfers.cancelled.insert(id);
        }
        let _ = self.send.send(Command::CancelSend(selection, id));
    }
}

struct Worker {
    db: Database,
    dir: PathBuf,
    inbox: Option<PathBuf>,
    identity: Identity,
    local: Local,
    connections: HashMap<String, Connection>,
    running: Option<RunningHost>,
    notice: Option<String>,
    copy: Option<String>,
    codex: String,
    model: Option<mpsc::Receiver<(ModelTask, Result<()>)>>,
    last_failed: Option<String>,
    retry_after: HashMap<String, Instant>,
    published: BTreeMap<String, bool>,
    device: DeviceSummary,
    transfers: Arc<Mutex<Transfers>>,
    upload_job: Option<UploadJob>,
    workspace_cancel: Option<(Uuid, Arc<std::sync::atomic::AtomicBool>)>,
    workspace_steering: Option<WorkspaceSteering>,
    workspace_progress: Option<Arc<Mutex<agent::app_server::TaskProgress>>>,
    task_activity_sent: BTreeMap<Uuid, (TaskActivity, Instant)>,
    task_tools: Option<Arc<agent::task_tools::TaskTools>>,
    admitted_messages: BTreeSet<(String, i64)>,
}

struct UploadJob {
    pin: String,
    id: Uuid,
    path: PathBuf,
    result: mpsc::Receiver<Result<Message>>,
}

struct WorkspaceSteering {
    job: Uuid,
    sender: mpsc::SyncSender<agent::app_server::SteerRequest>,
    pending: Option<(i64, mpsc::Receiver<bool>)>,
}

impl Worker {
    fn run(mut self, input: mpsc::Receiver<Command>, output: mpsc::Sender<View>) {
        for job in &mut self.local.jobs {
            if job.status == WorkspaceJobStatus::Running {
                job.status = if self.local.workspace_threads.contains_key(&job.id) {
                    WorkspaceJobStatus::Complete
                } else {
                    WorkspaceJobStatus::Failed("Uygulama kapanırken görev kesildi.".into())
                };
            }
        }
        if let Some(bind) = self.local.bind
            && let Err(error) = self.start(bind)
        {
            self.notice = Some(format!("Host başlatılamadı. {error}"));
        }
        if !self.publish(&output) {
            return;
        }
        let mut next = Instant::now();
        loop {
            let mut edited = false;
            match input.recv_timeout(Duration::from_millis(150)) {
                Ok(command) => {
                    if let Err(error) = self.execute(command) {
                        self.notice = Some(format!("{error:#}"));
                    }
                    edited = true;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if next <= Instant::now() {
                self.poll();
                self.publish_workspace_activity();
                next = Instant::now() + Duration::from_secs(2);
                edited = true;
            }
            if self.receive_upload() | self.receive_model() {
                edited = true;
            }
            #[cfg(any(windows, target_os = "linux"))]
            if self.deliver_task_desktop() {
                edited = true;
            }
            if self.admit_messages() {
                edited = true;
            }
            if self.advance_automation_commands() {
                edited = true;
            }
            if self.advance_workspace_control() {
                edited = true;
            }
            if self.advance_workspace_question() {
                edited = true;
            }
            if self.advance_workspace_steering() {
                edited = true;
            }
            let progress = self
                .workspace_progress
                .as_ref()
                .and_then(|progress| progress.lock().ok().map(|value| value.clone()));
            if self.local.workspace_progress != progress {
                if let Some(id) = self.workspace_cancel.as_ref().map(|(id, _)| *id)
                    && let Some(report) = &progress
                {
                    self.checkpoint_workspace(id, report);
                    self.local
                        .workspace_reports
                        .insert(id, Arc::new(report.clone()));
                }
                self.local.workspace_progress = progress;
                edited = true;
            }
            let approval = self.task_tools.as_ref().and_then(|gate| gate.pending());
            if self.local.task_approval != approval {
                self.local.task_approval = approval;
                edited = true;
            }
            #[cfg(any(windows, target_os = "linux"))]
            {
                let active = self.task_tools.as_ref().and_then(|tools| tools.window());
                let request = self
                    .task_tools
                    .as_ref()
                    .and_then(|tools| tools.window_request());
                if self.local.desktop_active != active || self.local.desktop_request != request {
                    self.local.desktop_active = active;
                    self.local.desktop_request = request;
                    edited = true;
                }
            }
            if self.schedule() {
                edited = true;
            }
            if edited && !self.publish(&output) {
                break;
            }
        }
        if let Some((_, cancel)) = &self.workspace_cancel {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn save(&self) -> Result<()> {
        self.db.execute("INSERT INTO client(id,data) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET data=excluded.data WHERE client.data <> excluded.data",
            params![serde_json::to_string(&self.local)?])?;
        Ok(())
    }

    fn publish(&mut self, output: &mpsc::Sender<View>) -> bool {
        if let Err(error) = self.save() {
            self.notice = Some(format!("Yerel kayıt başarısız. {error}"));
        }
        output
            .send(View {
                local: self.local.clone(),
                host: self.running.as_ref().map(|h| h.address.clone()),
                notice: self.notice.take(),
                copy: self.copy.take(),
                codex: self.codex.clone(),
            })
            .is_ok()
    }

    fn start(&mut self, bind: SocketAddr) -> Result<()> {
        if self
            .running
            .as_ref()
            .is_some_and(|host| host.address.socket == bind)
        {
            return Ok(());
        }
        ensure!(
            self.running.is_none(),
            "Adresi değiştirmek için önce hostu durdur"
        );
        let running = transport::start(&self.dir, bind, &self.identity)?;
        self.add_link(running.address.clone())?;
        self.local.bind = Some(running.address.socket);
        self.running = Some(running);
        Ok(())
    }

    fn add_link(&mut self, address: HostAddress) -> Result<()> {
        address.check()?;
        let key = address.pin.clone();
        if self
            .local
            .hosts
            .get(&key)
            .is_none_or(|link| link.address != address)
        {
            self.connections
                .insert(key.clone(), Connection::new(address.clone())?);
        }
        let entry = self
            .local
            .hosts
            .entry(key)
            .or_insert_with(|| Link::new(address.clone()));
        entry.address = address;
        Ok(())
    }

    fn connect(&mut self, pin: &str) -> Result<&mut Connection> {
        if !self.connections.contains_key(pin) {
            let address = self
                .local
                .hosts
                .get(pin)
                .context("Host bulunamadı")?
                .address
                .clone();
            self.connections
                .insert(pin.into(), Connection::new(address)?);
        }
        Ok(self.connections.get_mut(pin).expect("connection present"))
    }

    fn call<T: serde::de::DeserializeOwned>(&mut self, pin: &str, action: Action) -> Result<T> {
        let actor = self.identity.clone();
        self.connect(pin)?.call(&actor, action)
    }

    fn refresh(&mut self, pin: &str) -> Result<()> {
        let share_device = self.identity.card.data.role == Role::Agent
            && self.local.share_device.unwrap_or(true)
            && self.local.hosts[pin]
                .rooms
                .iter()
                .any(|room| !room.group && room.active && !room.closed);
        if self.published.get(pin) != Some(&share_device) {
            let publication = self.publication(share_device)?;
            let result = self.call::<Value>(
                pin,
                Action::PublishProfile {
                    publication: Box::new(publication),
                },
            );
            if result.is_ok()
                || result
                    .as_ref()
                    .is_err_and(|error| error.downcast_ref::<RemoteError>().is_some())
            {
                self.published.insert(pin.to_owned(), share_device);
            }
        }
        let after = self.local.hosts[pin].cursor;
        let read = self.local.hosts[pin]
            .seen
            .iter()
            .rev()
            .take(500)
            .map(|(&room, &seq)| (room, seq))
            .collect();
        let avatar = self
            .local
            .avatar
            .as_ref()
            .filter(|update| {
                update.data.as_deref().map(AvatarPhoto::digest)
                    != self.local.hosts[pin].avatar_current
            })
            .cloned();
        let sync: Sync = self.call(
            pin,
            Action::Sync {
                after,
                grants: self
                    .local
                    .grants
                    .iter()
                    .rev()
                    .filter(|g| g.proof.data.host == pin)
                    .take(20)
                    .cloned()
                    .collect(),
                read,
                avatar,
            },
        )?;
        let link = self.local.hosts.get_mut(pin).expect("link present");
        link.cursor = sync.cursor;
        link.rooms = sync.rooms;
        link.requests = sync.requests;
        link.receipts = sync.receipts;
        link.attachments = sync.supports_attachments;
        link.max_file_bytes = sync.max_file_bytes;
        link.agent_binary_files = sync.agent_binary_files;
        link.message_modes = sync.supports_message_modes;
        link.supports_task_activity = sync.supports_task_activity;
        link.task_activity = sync.task_activity;
        for ticket in sync.introductions {
            if !self.local.opened_invites.contains(&ticket.id)
                && !self.local.tickets.contains_key(&ticket.id)
            {
                self.local.incoming_introductions.insert(ticket.id, ticket);
            }
        }
        link.avatar_current = sync.avatar_index.get(self.identity.id()).cloned();
        link.avatars
            .retain(|actor, photo| sync.avatar_index.get(actor) == Some(&photo.digest));
        let missing_photo = sync
            .avatar_index
            .into_iter()
            .find(|(actor, _)| actor != self.identity.id() && !link.avatars.contains_key(actor));
        link.messages
            .extend(sync.messages.into_iter().map(|m| (m.seq, m)));
        for seq in sync.withdrawn_messages {
            redact_link(link, seq);
            self.local.held_messages.remove(&format!("{pin}/{seq}"));
        }
        link.online = true;
        link.error.clear();
        for request in &link.requests {
            for card in [&request.from, &request.to] {
                if card.data.key != self.identity.id() {
                    self.local
                        .contacts
                        .insert(card.data.key.clone(), card.clone());
                }
            }
        }
        if let Some((actor, digest)) = missing_photo
            && let Ok(photo) = self.call::<AvatarPhoto>(
                pin,
                Action::Avatar {
                    actor: actor.clone(),
                    digest: digest.clone(),
                },
            )
            && photo.digest == digest
            && AvatarPhoto::digest(&photo.data) == digest
            && AvatarPhoto::decode(&photo.data).is_ok()
        {
            self.local
                .hosts
                .get_mut(pin)
                .expect("link present")
                .avatars
                .insert(actor, photo);
        }
        Ok(())
    }

    fn publication(&self, share_device: bool) -> Result<ProfilePublication> {
        let profile = PublicProfile {
            display_name: self
                .local
                .display_name
                .clone()
                .unwrap_or_else(|| self.identity.card.data.name.clone()),
            agent: self.identity.agent_profile().cloned(),
        };
        let device = if share_device {
            Some(self.identity.sign("device-summary", self.device.clone())?)
        } else {
            None
        };
        Ok(ProfilePublication {
            profile: self.identity.sign("shared-profile", profile)?,
            device,
        })
    }

    fn lookup_profile(&mut self, actor: String) -> Result<()> {
        let trusted = self
            .local
            .friend(self.identity.id(), &actor)
            .map(|proof| proof.terms.data.host);
        let mut pins: Vec<_> = self
            .local
            .hosts
            .iter()
            .filter(|(_, link)| {
                link.online
                    && link
                        .rooms
                        .iter()
                        .any(|room| room.members.iter().any(|member| member.data.key == actor))
            })
            .map(|(pin, _)| pin.clone())
            .collect();
        pins.sort_by_key(|pin| Some(pin) != trusted.as_ref());
        self.local.profiles.remove(&actor);
        for pin in pins {
            if let Ok(Some(mut profile)) = self.call::<Option<ProfilePublication>>(
                &pin,
                Action::LoadProfile {
                    actor: actor.clone(),
                },
            ) {
                crate::identity::verify(&actor, "shared-profile", &profile.profile)?;
                if trusted.as_ref() == Some(&pin)
                    && let Some(device) = &profile.device
                {
                    crate::identity::verify(&actor, "device-summary", device)?;
                } else {
                    profile.device = None;
                }
                self.local.profiles.insert(actor, profile);
                break;
            }
        }
        Ok(())
    }

    fn poll(&mut self) {
        self.process_relationship_queue();
        if let Err(error) = self.receive_local_invite() {
            self.notice = Some(format!("Yerel davet alınamadı. {error}"));
        }
        let hosts: Vec<_> = self.local.hosts.keys().cloned().collect();
        for pin in hosts {
            if self
                .retry_after
                .get(&pin)
                .is_some_and(|t| *t > Instant::now())
            {
                continue;
            }
            match self.refresh(&pin) {
                Ok(()) => {
                    self.retry_after.remove(&pin);
                    if let Err(error) = self.flush(&pin) {
                        self.notice = Some(format!("{error:#}"));
                    }
                }
                Err(error) => {
                    let link = self.local.hosts.get_mut(&pin).expect("link present");
                    link.online = false;
                    link.error = error.to_string();
                    self.retry_after
                        .insert(pin, Instant::now() + Duration::from_secs(6));
                }
            }
        }
        if self.identity.card.data.role == Role::Agent {
            let ticket = self
                .local
                .hosts
                .values()
                .flat_map(|h| h.messages.values())
                .filter_map(|m| m.invitation.as_ref())
                .find(|t| {
                    !self.local.opened_invites.contains(&t.id)
                        && !self
                            .retry_after
                            .get(&t.host.pin)
                            .is_some_and(|retry| *retry > Instant::now())
                })
                .cloned();
            if let Some(ticket) = ticket {
                match self.redeem(ticket.clone()) {
                    Ok(()) => {}
                    Err(error) => {
                        if error.downcast_ref::<RemoteError>().is_some() {
                            self.local.opened_invites.insert(ticket.id);
                        } else {
                            self.retry_after
                                .insert(ticket.host.pin, Instant::now() + Duration::from_secs(6));
                        }
                    }
                }
            }
        }
    }

    fn process_relationship_queue(&mut self) {
        if self.running.is_some()
            && let Some((selection, card)) = self.local.pending_group_offers.first().cloned()
        {
            let result = self.offer(card, Some(selection));
            if !result.as_ref().is_err_and(authority_pending) {
                self.local.pending_group_offers.remove(0);
                if let Err(error) = result {
                    self.notice = Some(error.to_string());
                }
            } else {
                self.local.pending_group_offers.rotate_left(1);
            }
        }
        if let Some((host, request)) = self.local.pending_group_decisions.first().cloned() {
            let result = self.decide(&host, request, true);
            if !result.as_ref().is_err_and(authority_pending) {
                self.local.pending_group_decisions.remove(0);
                if let Err(error) = result {
                    self.notice = Some(error.to_string());
                }
            } else {
                self.local.pending_group_decisions.rotate_left(1);
            }
        }
        if let Some(selection) = self.local.removals.first().cloned()
            && self
                .call::<Value>(
                    &selection.host,
                    Action::EndFriendship {
                        room: selection.room,
                    },
                )
                .is_ok()
        {
            let _ = self.refresh(&selection.host);
            self.local.removals.remove(0);
        }
        if self.running.is_some()
            && let Some(route) = self.local.pending_contacts.first().cloned()
        {
            let target = route.data.card.data.key.clone();
            let result = self.offer_contact(&route);
            match result {
                Ok(ticket) => {
                    self.local.outbound_introductions.push((route, ticket));
                    self.local.pending_contact_tickets.remove(&target);
                    self.local.pending_contacts.remove(0);
                    let _ = self.save();
                }
                Err(error)
                    if ContactRoute::validate(&route).is_err()
                        || error
                            .downcast_ref::<RemoteError>()
                            .is_some_and(|e| e.status.is_client_error()) =>
                {
                    self.local.pending_contact_tickets.remove(&target);
                    self.local.pending_contacts.remove(0);
                    self.notice = Some(error.to_string());
                }
                Err(_) => {
                    self.local.pending_contacts.rotate_left(1);
                }
            }
        }
        if let Some((route, ticket)) = self.local.outbound_introductions.first().cloned()
            && (ContactRoute::validate(&route).is_err()
                || Connection::new(route.data.host.clone())
                    .and_then(|mut c| {
                        c.call::<Value>(
                            &self.identity,
                            Action::DeliverIntroduction {
                                route: Box::new(route),
                                ticket,
                            },
                        )
                    })
                    .is_ok())
        {
            self.local.outbound_introductions.remove(0);
            self.notice = Some("Kişi isteği eşitlendi.".into());
        }
        if let Some((id, ticket)) = self
            .local
            .incoming_introductions
            .iter()
            .next()
            .map(|(&id, ticket)| (id, ticket.clone()))
        {
            match self.redeem(ticket) {
                Ok(()) => {
                    self.local.incoming_introductions.remove(&id);
                }
                Err(error) if error.downcast_ref::<RemoteError>().is_some() => {
                    self.local.opened_invites.insert(id);
                    self.local.incoming_introductions.remove(&id);
                }
                Err(_) => {}
            }
        }
    }

    fn offer_contact(&mut self, route: &Signed<ContactRoute>) -> Result<Ticket> {
        ContactRoute::validate(route)?;
        let host = self
            .running
            .as_ref()
            .context("Bu cihazda host bekleniyor")?
            .address
            .clone();
        let target = &route.data.card.data.key;
        let (mut ticket, terms) =
            if let Some(prepared) = self.local.pending_contact_tickets.get(target) {
                prepared.clone()
            } else {
                let id = Uuid::new_v4();
                let ticket = Ticket {
                    host: host.clone(),
                    id,
                    code: encode(crate::identity::random::<32>()?),
                    grant: None,
                };
                let terms = self.identity.sign(
                    "offer",
                    Terms {
                        id,
                        host: host.pin.clone(),
                        from: self.identity.id().into(),
                        to: target.clone(),
                        room: None,
                        expires: route.data.expires.min(now() + INVITE_LIFETIME),
                    },
                )?;
                self.local
                    .pending_contact_tickets
                    .insert(target.clone(), (ticket.clone(), terms.clone()));
                self.save()?;
                (ticket, terms)
            };
        ensure!(ticket.host.pin == host.pin, "İsteğin host kimliği değişti");
        ticket.host = host.clone();
        let _: Request = self.call(
            &host.pin,
            Action::Offer {
                terms,
                target: route.data.card.clone(),
                friendship: None,
                authority: None,
                code_hash: crate::host::digest(&ticket.code),
            },
        )?;
        self.local.tickets.insert(ticket.id, ticket.clone());
        Ok(ticket)
    }

    fn receive_local_invite(&mut self) -> Result<()> {
        let Some(root) = &self.inbox else {
            return Ok(());
        };
        let entries = fs::read_dir(root.join(self.identity.id()))?;
        for entry in entries.flatten().take(32) {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("invite") {
                continue;
            }
            let Ok(contents) =
                open_regular(&path, false).and_then(|file| read_bounded(&file, 32_768))
            else {
                continue;
            };
            let Ok(contents) = std::str::from_utf8(&contents) else {
                continue;
            };
            let Ok(ticket) = unpack::<Ticket>("invite", contents) else {
                continue;
            };
            if ticket.grant.is_some()
                || ticket.host.check().is_err()
                || path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .and_then(|stem| Uuid::parse_str(stem).ok())
                    != Some(ticket.id)
            {
                continue;
            }
            if self
                .retry_after
                .get(&ticket.host.pin)
                .is_some_and(|retry| *retry > Instant::now())
            {
                continue;
            }
            let pin = ticket.host.pin.clone();
            match self.redeem(ticket) {
                Ok(()) => {
                    self.save()?;
                    fs::remove_file(path)?;
                    self.notice = Some(
                        "Yeni davet geldi. Kişiler ve istekler bölümünden açabilirsin.".into(),
                    );
                }
                Err(error)
                    if error
                        .downcast_ref::<RemoteError>()
                        .is_some_and(|remote| remote.status.is_client_error()) =>
                {
                    fs::remove_file(path)?;
                    self.notice = Some(format!("Yerel davet açılamadı. {error}"));
                }
                Err(_) => {
                    self.retry_after
                        .insert(pin, Instant::now() + Duration::from_secs(6));
                }
            }
            break;
        }
        Ok(())
    }

    fn send_draft(
        &mut self,
        s: Selection,
        client_id: Uuid,
        text: String,
        reply: Option<i64>,
        path: Option<PathBuf>,
        mode: Option<MessageMode>,
    ) -> Result<()> {
        ensure!(
            mode.is_none()
                || self
                    .local
                    .hosts
                    .get(&s.host)
                    .is_some_and(|link| link.message_modes),
            "Host komut seçimlerini desteklemiyor. Taslağı koruyup hostu güncelle."
        );

        ensure!(
            !self.local.removals.contains(&s),
            "Arkadaşlığı kaldırma bekliyor"
        );
        ensure!(
            (path.is_some() || !text.trim().is_empty()) && text.chars().count() <= MAX_TEXT,
            "Mesaj 4.000 karakteri aşamaz ve dosya eki yoksa boş olamaz"
        );
        let link = self.local.hosts.get(&s.host).context("Host bulunamadı")?;
        let binary = path
            .as_ref()
            .filter(|_| link.max_file_bytes >= MAX_BINARY_ATTACHMENT)
            .cloned();
        if let Some(path) = &binary {
            ensure!(
                link.queue.iter().filter(|p| p.binary.is_some()).count() < 8,
                "Bu hostta bekleyen dosyalar var"
            );
            ensure!(
                !link.queue.iter().any(|p| p.draft.id == client_id),
                "Gönderim zaten kuyrukta"
            );
            let metadata = fs::metadata(path)?;
            ensure!(
                metadata.is_file() && (1..=MAX_BINARY_ATTACHMENT as u64).contains(&metadata.len()),
                "Dosya 32 MiB sınırını aşıyor"
            );
            check_filename(
                path.file_name()
                    .and_then(|name| name.to_str())
                    .context("Geçersiz dosya adı")?,
            )?;
        }
        let attachment = path
            .filter(|_| binary.is_none())
            .map(|path| {
                let link = self.local.hosts.get(&s.host).context("Host bulunamadı")?;
                ensure!(link.attachments, "Bu host dosya eklerini desteklemiyor");
                ensure!(
                    link.queue
                        .iter()
                        .filter(|item| item.draft.attachment.is_some())
                        .count()
                        < 8,
                    "Bu hostta bekleyen dosyalar var. Önce onların gönderilmesini bekle."
                );
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .context("Geçersiz dosya adı")?;
                let file = fs::File::open(&path)?;
                ensure!(file.metadata()?.is_file(), "Bir dosya seç");
                let mut bytes = Vec::new();
                file.take((MAX_ATTACHMENT + 1) as u64)
                    .read_to_end(&mut bytes)?;
                ensure!(
                    !bytes.is_empty() && bytes.len() <= MAX_ATTACHMENT,
                    "Dosya boyutu 1 bayt ile 1 MiB arasında olmalı"
                );
                let attachment = Attachment {
                    name: name.to_owned(),
                    data: encode(bytes),
                };
                attachment.bytes()?;
                Ok::<_, anyhow::Error>(attachment)
            })
            .transpose()?;
        if let Some(binary) = binary {
            self.local
                .hosts
                .get_mut(&s.host)
                .context("Host bulunamadı")?
                .queue
                .push(Pending {
                    draft: Draft {
                        mode: mode.clone(),
                        id: client_id,
                        room: s.room,
                        text,
                        reply,
                        invitation: None,
                        attachment: None,
                    },
                    error: None,
                    binary: Some(binary),
                    task_question: false,
                });
            self.save()?;
        } else {
            self.queue(
                &s.host,
                Draft {
                    mode: mode.clone(),
                    id: client_id,
                    room: s.room,
                    text,
                    reply,
                    invitation: None,
                    attachment,
                },
            )?;
        }
        if self.local.hosts[&s.host].online {
            self.flush(&s.host)?;
        }
        Ok(())
    }

    fn queue(&mut self, pin: &str, draft: Draft) -> Result<()> {
        self.queue_entry(
            pin,
            Pending {
                draft,
                error: None,
                binary: None,
                task_question: false,
            },
        )
    }

    fn queue_entry(&mut self, pin: &str, entry: Pending) -> Result<()> {
        let link = self.local.hosts.get_mut(pin).context("Host bulunamadı")?;
        link.queue.push(entry);
        if let Err(error) = self.save() {
            self.local
                .hosts
                .get_mut(pin)
                .expect("queued host")
                .queue
                .pop();
            return Err(error);
        }
        Ok(())
    }

    fn queue_task_result(
        &mut self,
        job: &WorkspaceJob,
        result: agent::app_server::TaskResult,
    ) -> Result<()> {
        let link = &self.local.hosts[&job.selection.host];
        let mut binary = None;
        let attachment = if let Some(file) = result.file {
            ensure!(
                link.queue
                    .iter()
                    .filter(|entry| entry.binary.is_some() || entry.draft.attachment.is_some())
                    .count()
                    < 8,
                "Bu hostta bekleyen dosyalar var"
            );
            if link.agent_binary_files && link.max_file_bytes > 0 {
                ensure!(
                    file.bytes.len() <= link.max_file_bytes,
                    "Host dosya sınırı değişti"
                );
                let path = self
                    .dir
                    .join("task-outputs")
                    .join(job.id.to_string())
                    .join(&file.name);
                write_new(&path, &file.bytes)?;
                binary = Some(path);
                None
            } else {
                ensure!(
                    link.attachments && file.bytes.len() <= MAX_ATTACHMENT,
                    "Host bu görev dosyasını desteklemiyor"
                );
                Some(Attachment {
                    name: file.name,
                    data: encode(file.bytes),
                })
            }
        } else {
            result.screenshot.map(|image| Attachment {
                name: "Ekran görüntüsü.jpg".into(),
                data: encode(image),
            })
        };
        let queued = self.queue_entry(
            &job.selection.host,
            Pending {
                draft: Draft {
                    mode: None,
                    id: job.id,
                    room: job.selection.room,
                    text: result.text,
                    reply: job.automation.is_none().then_some(job.source),
                    invitation: None,
                    attachment,
                },
                error: None,
                binary: binary.clone(),
                task_question: false,
            },
        );
        if queued.is_err()
            && let Some(path) = binary
        {
            self.remove_task_output(job.id, &path);
        }
        queued
    }

    #[cfg(any(windows, target_os = "linux"))]
    fn deliver_task_desktop(&mut self) -> bool {
        let Some(delivery) = self
            .task_tools
            .as_ref()
            .and_then(|tools| tools.take_desktop_delivery())
        else {
            return false;
        };
        let result = (|| {
            let (job_id, cancelled) = self.workspace_cancel.as_ref().context("Görev sona erdi")?;
            ensure!(
                !cancelled.load(std::sync::atomic::Ordering::Acquire),
                "Görev durduruldu"
            );
            let job = self
                .local
                .jobs
                .iter()
                .find(|job| job.id == *job_id)
                .cloned()
                .context("Görüntünün görevi bulunamadı")?;
            ensure!(
                self.workspace_authorized(&job),
                "Görev sohbeti artık yetkili değil"
            );
            let message: Message = self.call(
                &job.selection.host,
                Action::Send {
                    draft: Draft {
                        mode: None,
                        id: desktop_delivery_id(&job),
                        room: job.selection.room,
                        text: "Masaüstü ekran görüntüsü".into(),
                        reply: None,
                        invitation: None,
                        attachment: Some(Attachment {
                            name: DESKTOP_SCREENSHOT_NAME.into(),
                            data: encode(delivery.image),
                        }),
                    },
                },
            )?;
            ensure!(
                desktop_delivery_acknowledged(self.identity.id(), &job, &message),
                "Host görüntü ekini doğrulamadı"
            );
            self.local
                .hosts
                .get_mut(&job.selection.host)
                .expect("known host")
                .messages
                .insert(message.seq, message);
            self.save()
        })();
        let _ = delivery.reply.send(result);
        true
    }

    fn remove_task_output(&self, id: Uuid, path: &std::path::Path) {
        let parent = self.dir.join("task-outputs").join(id.to_string());
        if path.parent() == Some(parent.as_path()) {
            let _ = fs::remove_file(path);
            let _ = fs::remove_dir(parent);
        }
    }

    fn flush(&mut self, pin: &str) -> Result<()> {
        let question = self
            .task_tools
            .as_ref()
            .and_then(|tools| tools.question())
            .map(|question| question.id);
        self.local
            .hosts
            .get_mut(pin)
            .expect("link present")
            .queue
            .retain(|entry| !entry.task_question || question == Some(entry.draft.id));
        for _ in 0..20 {
            let Some(entry) = self.local.hosts[pin]
                .queue
                .iter()
                .find(|p| p.error.is_none())
                .cloned()
            else {
                break;
            };
            if let Some(path) = entry.binary {
                if self.upload_job.is_none() {
                    let upload_path = path.clone();
                    let address = self.local.hosts[pin].address.clone();
                    let identity = self.identity.clone();
                    let state = self.transfers.clone();
                    let draft = entry.draft;
                    let id = draft.id;
                    let (send, result) = mpsc::channel();
                    self.transfers
                        .lock()
                        .expect("file progress lock")
                        .active
                        .insert(id);
                    let started =
                        thread::Builder::new()
                            .name("chatoid-upload".into())
                            .spawn(move || {
                                let result = Self::upload_attachment(
                                    address,
                                    &identity,
                                    &state,
                                    draft,
                                    &upload_path,
                                );
                                let _ = send.send(result);
                            });
                    if let Err(error) = started {
                        self.transfers
                            .lock()
                            .expect("file progress lock")
                            .active
                            .remove(&id);
                        return Err(error.into());
                    }
                    self.upload_job = Some(UploadJob {
                        pin: pin.into(),
                        id,
                        path,
                        result,
                    });
                }
                break;
            }
            let result: Result<Message> = self.call(
                pin,
                Action::Send {
                    draft: entry.draft.clone(),
                },
            );
            let link = self.local.hosts.get_mut(pin).expect("link present");
            match result {
                Ok(message) => {
                    link.queue.retain(|p| p.draft.id != entry.draft.id);
                    link.messages.insert(message.seq, message);
                }
                Err(error)
                    if error
                        .downcast_ref::<RemoteError>()
                        .is_some_and(|e| e.status.is_client_error()) =>
                {
                    if let Some(pending) =
                        link.queue.iter_mut().find(|p| p.draft.id == entry.draft.id)
                    {
                        pending.error = Some(error.to_string());
                    }
                }
                Err(error) => {
                    link.online = false;
                    link.error = error.to_string();
                    break;
                }
            }
            self.save()?;
        }
        Ok(())
    }

    fn receive_upload(&mut self) -> bool {
        let Some(job) = self.upload_job.as_ref() else {
            return false;
        };
        let result = match job.result.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => Err(anyhow::anyhow!("Dosya aktarımı kesildi")),
        };
        let job = self.upload_job.take().expect("upload ready");
        let cancelled = {
            let mut state = self.transfers.lock().expect("file progress lock");
            state.active.remove(&job.id);
            state.progress.remove(&job.id);
            state.cancelled.remove(&job.id)
        };
        let dispose_output = result.is_ok() || cancelled;
        let Some(link) = self.local.hosts.get_mut(&job.pin) else {
            return true;
        };
        match result {
            Ok(message) => {
                link.queue.retain(|pending| pending.draft.id != job.id);
                link.messages.insert(message.seq, message);
            }
            Err(_) if cancelled => {
                link.queue.retain(|pending| pending.draft.id != job.id);
            }
            Err(error) if error.is::<reqwest::Error>() => {
                link.online = false;
                link.error = error.to_string();
                self.retry_after
                    .insert(job.pin, Instant::now() + Duration::from_secs(6));
            }
            Err(error) => {
                if let Some(pending) = link.queue.iter_mut().find(|p| p.draft.id == job.id) {
                    pending.error = Some(error.to_string());
                }
            }
        }
        if dispose_output && self.save().is_ok() {
            self.remove_task_output(job.id, &job.path);
        }
        true
    }

    fn upload_attachment(
        address: HostAddress,
        identity: &Identity,
        transfers: &Mutex<Transfers>,
        draft: Draft,
        path: &std::path::Path,
    ) -> Result<Message> {
        use sha2::{Digest, Sha256};
        let file = fs::File::open(path)?;
        let size = usize::try_from(file.metadata()?.len())?;
        ensure!(
            (1..=MAX_BINARY_ATTACHMENT).contains(&size),
            "Dosya boyutu değişti"
        );
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("Dosya adı bulunamadı")?
            .to_owned();
        check_filename(&name)?;
        let mut connection = Connection::new(address)?;
        let mut file = file;
        let mut hash = Sha256::new();
        let mut offset = 0;
        let mut buffer = vec![0; FILE_CHUNK];
        while offset < size {
            {
                let mut state = transfers.lock().expect("file progress lock");
                ensure!(
                    !state.cancelled.contains(&draft.id),
                    "Dosya gönderimi iptal edildi"
                );
                state.progress.insert(draft.id, (offset, size));
            }
            let amount = (size - offset).min(FILE_CHUNK);
            let chunk = &mut buffer[..amount];
            file.read_exact(chunk)?;
            hash.update(&*chunk);
            connection.upload_chunk(identity, draft.id, offset, chunk)?;
            offset += amount;
        }
        ensure!(
            !transfers
                .lock()
                .expect("file progress lock")
                .cancelled
                .contains(&draft.id),
            "Dosya gönderimi iptal edildi"
        );
        connection.call(
            identity,
            Action::SendBinary {
                draft,
                name,
                bytes: size,
                hash: encode(hash.finalize()),
            },
        )
    }

    fn redeem(&mut self, ticket: Ticket) -> Result<()> {
        let pin = ticket.host.pin.clone();
        let mut connection = Connection::new(ticket.host.clone())?;
        let request: Request = connection.call(
            &self.identity,
            Action::Redeem {
                id: ticket.id,
                code: ticket.code.clone(),
                grant: ticket.grant.clone(),
            },
        )?;
        self.add_link(ticket.host.clone())?;
        self.connections.insert(pin.clone(), connection);
        if let Some(grant) = &ticket.grant
            && !self.local.grants.contains(grant)
        {
            self.local.grants.push(grant.clone());
        }
        self.local.opened_invites.insert(ticket.id);
        self.local.tickets.insert(ticket.id, ticket);
        self.local
            .contacts
            .insert(request.from.data.key.clone(), request.from);
        self.refresh(&pin)
    }

    fn offer(&mut self, card: Card, room: Option<Selection>) -> Result<Ticket> {
        let actor = self.identity.clone();
        let host = self
            .running
            .as_ref()
            .context("Önce bu cihazda hostu başlat")?
            .address
            .clone();
        let group = if let Some(selection) = &room {
            ensure!(
                host.pin == selection.host,
                "Grup bu cihazda barındırılmıyor"
            );
            let proof = self
                .local
                .friend(actor.id(), &card.data.key)
                .context("Önce arkadaşlık isteği kabul edilmeli")?;
            Some((selection.room, proof))
        } else {
            ensure!(
                !self.local.already_requested(actor.id(), &card.data.key),
                "Bu hesapla zaten bir sohbet veya istek var"
            );
            None
        };
        let authority = group.as_ref().and_then(|(_, proof)| {
            self.local
                .hosts
                .get(&proof.terms.data.host)
                .map(|link| link.address.clone())
        });
        let (request, ticket) = self.connect(&host.pin)?.offer_with_authority(
            &actor,
            card.clone(),
            group,
            authority,
        )?;
        let issued = ticket.clone();
        self.local.tickets.insert(ticket.id, ticket.clone());
        let code = pack("invite", &ticket)?;
        let delivered = self.inbox.as_ref().is_some_and(|root| {
            let recipient = root.join(&request.to.data.key);
            recipient.is_dir()
                && write_new(
                    &recipient.join(format!("{}.invite", ticket.id)),
                    code.as_bytes(),
                )
                .is_ok()
        });
        self.copy = Some(code);
        self.notice = Some(
            if delivered {
                "Davet bu cihazdaki alıcıya gönderildi."
            } else {
                "Davet bağlantısı panoya kopyalandı."
            }
            .into(),
        );
        if room.is_some() {
            let dm = self.local.hosts.iter().find_map(|(pin, link)| {
                link.rooms
                    .iter()
                    .find(|r| {
                        !r.group
                            && r.active
                            && r.members.iter().any(|m| m.data.key == card.data.key)
                    })
                    .map(|r| (pin.clone(), r.id))
            });
            if let Some((pin, dm)) = dm {
                self.queue(
                    &pin,
                    Draft {
                        mode: None,
                        id: Uuid::new_v4(),
                        room: dm,
                        text: format!(
                            "Grup daveti · {}",
                            request.room_name.as_deref().unwrap_or_default()
                        ),
                        reply: None,
                        invitation: Some(ticket),
                        attachment: None,
                    },
                )?;
            }
        }
        self.refresh(&host.pin)?;
        Ok(issued)
    }

    fn decide(&mut self, host: &str, request: Request, accepted: bool) -> Result<()> {
        let terms = request.terms.data;
        let grant = if terms.to != self.identity.id() {
            Some(
                self.local
                    .grants
                    .iter()
                    .find(|g| {
                        g.agent.data.key == terms.to
                            && g.proof.data.host == host
                            && g.proof.data.request == terms.id
                    })
                    .context("Ajan operatör yetkisi yok")?
                    .clone(),
            )
        } else {
            None
        };
        if accepted && terms.room.is_none() {
            let conflicting = self.local.hosts.values().any(|h| {
                h.requests.iter().any(|r| {
                    let other = &r.terms.data;
                    other.id != terms.id
                        && other.room.is_none()
                        && ((other.from == terms.from && other.to == terms.to)
                            || (other.from == terms.to && other.to == terms.from))
                        && (self.local.friendship_open(h, r)
                            || (r.status == Status::Pending
                                && (other.from.as_str(), other.id)
                                    < (terms.from.as_str(), terms.id)))
                })
            });
            ensure!(
                !conflicting,
                "Bu hesapla daha önce açılmış bir istek veya sohbet var"
            );
        }
        let decision = Decision::issue(&self.identity, terms, accepted, grant)?;
        let saved: Request = self.call(host, Action::Decide { decision })?;
        self.notice = Some(
            if saved.status == Status::Accepted {
                "İstek kabul edildi."
            } else {
                "İstek reddedildi."
            }
            .into(),
        );
        self.refresh(host)
    }

    fn execute(&mut self, command: Command) -> Result<()> {
        match command {
            Command::AddTaskMcp(setup) => {
                ensure!(
                    self.identity.card.data.role == Role::Agent,
                    "Ajan hesabı gerekli"
                );
                ensure!(self.model.is_none(), "Codex meşgul");
                let settings = self.local.settings.clone();
                let home = self.dir.join("codex-tasks");
                self.spawn_model(move || {
                    let result = agent::app_server::add_mcp_connection(&settings, &home, &setup);
                    (
                        ModelTask::McpAdded {
                            codex: settings.codex,
                            name: setup.name,
                            result,
                        },
                        Ok(()),
                    )
                });
            }
            Command::SignInMcp(server) => {
                ensure!(
                    self.identity.card.data.role == Role::Agent,
                    "Ajan hesabı gerekli"
                );
                ensure!(self.model.is_none(), "Codex meşgul");
                ensure!(
                    self.local.task_mcp.as_ref().is_some_and(|connections| {
                        connections.iter().any(|connection| {
                            connection.name == server && connection.auth_status != "unsupported"
                        })
                    }),
                    "Önce OAuth destekleyen bir MCP bağlantısını listele"
                );
                let settings = self.local.settings.clone();
                let home = self.dir.join("codex-tasks");
                self.spawn_model(move || {
                    let result = agent::sign_in_mcp(&settings.codex, &home, &server)
                        .and_then(|_| agent::app_server::list_mcp_connections(&settings, &home));
                    (
                        ModelTask::McpSignIn {
                            codex: settings.codex,
                            server,
                            result,
                        },
                        Ok(()),
                    )
                });
                self.codex = "MCP girişi tarayıcıda bekleniyor".into();
            }
            Command::LoadTaskMcp => {
                ensure!(
                    self.identity.card.data.role == Role::Agent,
                    "Ajan hesabı gerekli"
                );
                ensure!(self.model.is_none(), "Codex meşgul");
                let settings = self.local.settings.clone();
                let home = self.dir.join("codex-tasks");
                self.spawn_model(move || {
                    let result = agent::app_server::list_mcp_connections(&settings, &home);
                    (
                        ModelTask::McpCatalog {
                            codex: settings.codex,
                            result,
                        },
                        Ok(()),
                    )
                });
            }
            Command::LoadTaskModels => {
                ensure!(
                    self.identity.card.data.role == Role::Agent,
                    "Ajan hesabı gerekli"
                );
                ensure!(self.model.is_none(), "Codex meşgul");
                let settings = self.local.settings.clone();
                let home = self.dir.join("codex-tasks");
                self.spawn_model(move || {
                    let result = agent::app_server::list_models(&settings, &home);
                    (
                        ModelTask::Catalog {
                            codex: settings.codex,
                            result,
                        },
                        Ok(()),
                    )
                });
            }
            Command::LoadTaskSkills => {
                ensure!(
                    self.identity.card.data.role == Role::Agent,
                    "Ajan hesabı gerekli"
                );
                ensure!(self.model.is_none(), "Codex meşgul");
                let settings = self.local.settings.clone();
                ensure!(!settings.workspace.is_empty(), "Önce çalışma klasörünü seç");
                let home = self.dir.join("codex-tasks");
                self.spawn_model(move || {
                    let result = agent::app_server::list_skills(
                        &settings,
                        &home,
                        std::path::Path::new(&settings.workspace),
                    );
                    (
                        ModelTask::SkillCatalog {
                            codex: settings.codex,
                            directory: settings.workspace,
                            roots: settings.skill_roots,
                            result,
                        },
                        Ok(()),
                    )
                });
            }
            Command::DecideWorkspaceJob(id, decision) => self.decide_workspace_job(id, decision)?,
            Command::RetryWorkspaceJob(id) => {
                let index = self
                    .local
                    .jobs
                    .iter()
                    .position(|job| job.id == id)
                    .context("Görev bulunamadı")?;
                let job = &self.local.jobs[index];
                ensure!(
                    matches!(
                        job.status,
                        WorkspaceJobStatus::Failed(_) | WorkspaceJobStatus::Cancelled
                    ),
                    "Yalnızca kesilen veya başarısız görev yeniden denenebilir"
                );
                ensure!(
                    self.workspace_authorized(job) && self.workspace_host_online(job),
                    "Görev sohbeti veya operatörü artık yetkili değil"
                );
                let link = &self.local.hosts[&job.selection.host];
                ensure!(
                    !self.local.workspace_threads.contains_key(&id)
                        && !link.queue.iter().any(|entry| entry.draft.id == id)
                        && !link
                            .messages
                            .values()
                            .any(|message| message.client_id == id),
                    "Bu görevin sonucu gönderildi veya gönderim kuyruğunda. Gönderimi yeniden dene."
                );
                ensure!(
                    job.directory == self.task_directory(),
                    "Çalışma klasörü değişti. Yeni görev gönder."
                );
                let next_round = job
                    .approval_round
                    .checked_add(1)
                    .context("Görev yeniden deneme sınırına ulaştı")?;
                let previous = self.local.jobs[index].clone();
                self.local.jobs[index].approval_round = next_round;
                if let Some(recovery) = self.local.jobs[index].recovery.clone() {
                    self.local.jobs[index].continuation = Some(recovery);
                }
                self.local.jobs[index].status = WorkspaceJobStatus::Pending;
                self.local.jobs[index].model = None;
                self.local.jobs[index].review = None;
                self.local.jobs[index].plan_only = false;
                self.local.jobs[index].web_research = false;
                self.local.jobs[index].network = false;
                self.local.jobs[index].delegate = false;
                self.local.jobs[index].skill_names.clear();
                self.local.jobs[index].forked = false;
                if let Err(error) = self.save() {
                    self.local.jobs[index] = previous;
                    return Err(error);
                }
                self.notice = Some(
                    "Görev yeniden onay bekliyor. Önceki denemede dosyalar değişmiş olabilir."
                        .into(),
                );
            }
            Command::CancelWorkspaceJob(id) => {
                let (_, cancel) = self
                    .workspace_cancel
                    .as_ref()
                    .filter(|(job_id, _)| *job_id == id)
                    .context("Bu görev çalışmıyor")?;
                cancel.store(true, std::sync::atomic::Ordering::Release);
            }
            #[cfg(any(windows, target_os = "linux"))]
            Command::SetDesktopTarget(target) => {
                ensure!(
                    self.identity.card.data.role == Role::Agent && self.workspace_cancel.is_none(),
                    "Pencere seçimi görev başlamadan yapılır"
                );
                self.local.desktop_target = target;
            }
            #[cfg(any(windows, target_os = "linux"))]
            Command::DecideDesktopTarget(id, target) => {
                let gate = self
                    .task_tools
                    .as_ref()
                    .context("Çalışan Codex görevi yok")?;
                if target.is_some() {
                    self.ensure_current_job_access()?;
                }
                gate.choose_window(id, target)?;
            }
            Command::DecideTaskAction(id, accept) => {
                let gate = self
                    .task_tools
                    .as_ref()
                    .context("Çalışan Codex görevi yok")?;
                if accept {
                    self.ensure_current_job_access()?;
                }
                gate.decide(id, accept)?;
            }
            Command::Start(bind) => self.start(bind)?,
            Command::Stop => {
                self.running.take();
                self.local.bind = None;
            }
            Command::Import(code) if code.trim().starts_with("chatoid:card:") => {
                let card: Card = unpack("card", &code)?;
                card.check()?;
                ensure!(card.data.key != self.identity.id(), "Bu senin kişi kartın");
                self.notice = Some(format!("{} kişilere eklendi.", card.data.name));
                self.local.contacts.insert(card.data.key.clone(), card);
            }
            Command::Import(code) if code.trim().starts_with("chatoid:host:") => {
                let address: HostAddress = unpack("host", &code)?;
                let mut connection = Connection::new(address.clone())?;
                let pin = address.pin.clone();
                let known = self.local.hosts.get(&pin);
                let sync: Sync = connection.call(
                    &self.identity,
                    Action::Sync {
                        after: known.map_or(0, |link| link.cursor),
                        grants: vec![],
                        read: BTreeMap::new(),
                        avatar: None,
                    },
                )?;
                ensure!(
                    known.is_some() || !sync.rooms.is_empty() || !sync.requests.is_empty(),
                    "Bu hostta bu hesaba ait kayıt yok. İlk bağlantı için davet kodu gerekli."
                );
                let was_known = known.is_some();
                self.add_link(address)?;
                self.connections.insert(pin.clone(), connection);
                self.refresh(&pin)?;
                self.notice = Some(if was_known {
                    "Host adresi güncellendi.".into()
                } else {
                    "Hosta yeniden bağlanıldı.".into()
                });
            }
            Command::Import(code) => self.redeem(unpack("invite", &code)?)?,
            Command::Pair(code) => {
                let operator: Card = unpack("card", &code)?;
                operator.check()?;
                ensure!(
                    self.identity.card.data.role == Role::Agent
                        && operator.data.role == Role::Human,
                    "Operatör için insan hesabı gerekli"
                );
                self.local.operator = Some(operator);
                self.notice = Some("Operatör eşleştirildi.".into());
            }
            Command::Offer(card) => {
                self.offer(card, None)?;
            }
            Command::OfferContact(route) => {
                ContactRoute::validate(&route)?;
                ensure!(
                    route.data.card.data.key != self.identity.id(),
                    "Kendi hesabına istek gönderemezsin"
                );
                ensure!(
                    !self
                        .local
                        .already_requested(self.identity.id(), &route.data.card.data.key),
                    "Bu hesapla zaten bir sohbet veya istek var"
                );
                if !self
                    .local
                    .pending_contacts
                    .iter()
                    .any(|item| item.data.card.data.key == route.data.card.data.key)
                {
                    self.local.pending_contacts.push(route);
                }
                self.notice = Some(
                    if self.running.is_some() {
                        "İstek teslim bekliyor."
                    } else {
                        "İstek kaydedildi. Göndermek için bu cihazda hostu aç."
                    }
                    .into(),
                );
            }
            Command::ShareContact => {
                let host = self
                    .running
                    .as_ref()
                    .context("Bağlantımı paylaşmak için önce bu cihazda hostu aç")?
                    .address
                    .clone();
                let local = host.socket.ip().is_loopback();
                let route = self.identity.sign(
                    "contact-route",
                    ContactRoute {
                        card: self.identity.card.clone(),
                        host,
                        expires: now() + INVITE_LIFETIME,
                    },
                )?;
                self.copy = Some(pack("contact", &route)?);
                self.notice = Some(
                    if local {
                        "Bağlantı kopyalandı · Bu cihazda kullanılır."
                    } else {
                        "Kişi bağlantısı kopyalandı."
                    }
                    .into(),
                );
            }
            Command::EndFriendship(selection) => {
                if !self.local.removals.contains(&selection) {
                    self.local.removals.push(selection);
                }
                self.notice = Some("Arkadaşlığı kaldırma bekliyor.".into());
            }
            Command::Group(name) => {
                let pin = self
                    .running
                    .as_ref()
                    .context("Önce bu cihazda hostu başlat")?
                    .address
                    .pin
                    .clone();
                let _: Uuid = self.call(
                    &pin,
                    Action::Group {
                        id: Uuid::new_v4(),
                        name,
                    },
                )?;
                self.refresh(&pin)?;
            }
            Command::Invite(selection, card) => {
                match self.offer(card.clone(), Some(selection.clone())) {
                    Ok(_) => {}
                    Err(error) if authority_pending(&error) => {
                        if !self
                            .local
                            .pending_group_offers
                            .iter()
                            .any(|(room, member)| room == &selection && member == &card)
                        {
                            self.local.pending_group_offers.push((selection, card));
                        }
                        self.notice = Some(error.to_string());
                    }
                    Err(error) => return Err(error),
                }
            }
            Command::Decide(host, request, accepted) => {
                self.local
                    .pending_group_decisions
                    .retain(|(_, item)| item.terms.data.id != request.terms.data.id);
                match self.decide(&host, (*request).clone(), accepted) {
                    Err(error)
                        if accepted
                            && request.terms.data.room.is_some()
                            && authority_pending(&error) =>
                    {
                        self.local.pending_group_decisions.push((host, *request));
                        self.notice = Some(error.to_string());
                    }
                    result => result?,
                }
            }
            Command::SendDraft(s, id, text, reply, path) => {
                self.send_draft(s, id, text, reply, path, None)?;
            }
            Command::SendModeDraft(s, id, text, reply, path, mode) => {
                self.send_draft(s, id, text, reply, path, mode)?;
            }
            Command::WithdrawMessage(selection, seq) => {
                self.call::<Value>(
                    &selection.host,
                    Action::WithdrawMessage {
                        room: selection.room,
                        seq,
                    },
                )?;
                if let Some(link) = self.local.hosts.get_mut(&selection.host) {
                    redact_link(link, seq);
                }
                self.notice = Some("Mesaj geri alındı.".into());
            }
            Command::Download(s, seq, path) => {
                let message = self
                    .local
                    .hosts
                    .get(&s.host)
                    .and_then(|link| link.messages.get(&seq))
                    .filter(|m| m.room == s.room)
                    .cloned()
                    .context("Dosya bilgisi bulunamadı")?;
                let parent = path.parent().context("Hedef klasör bulunamadı")?;
                let mut output = tempfile::NamedTempFile::new_in(parent)?;
                let identity = self.identity.clone();
                self.connect(&s.host)?.download_attachment(
                    &identity,
                    &message,
                    &mut output,
                    || Ok(()),
                )?;
                output.as_file().sync_all()?;
                output.persist(&path).map_err(|error| error.error)?;
                self.notice = Some(format!("Dosya kaydedildi · {}", path.display()));
            }
            Command::SetAvatar(path) => {
                let data = path.as_deref().map(prepare_avatar).transpose()?;
                self.local.avatar = Some(AvatarUpdate { data });
                self.notice = Some("Profil fotoğrafı güncellendi.".into());
            }
            Command::SetBuiltInAvatar(index) => {
                ensure!(index < AVATAR_PRESETS.len(), "Avatar bulunamadı");
                let [r, g, b] = AVATAR_PRESETS[index];
                let image = image::RgbImage::from_fn(AVATAR_PX, AVATAR_PX, |x, y| {
                    let (dx, dy) = (x as i32 - 40, y as i32 - 40);
                    let shape = match index % 3 {
                        0 => dx * dx + dy * dy < 24 * 24,
                        1 => dx.abs() + dy.abs() < 32,
                        _ => dx.abs() < 20 && dy.abs() < 20,
                    };
                    if shape {
                        image::Rgb([r, g, b])
                    } else {
                        image::Rgb([27, 29, 43])
                    }
                });
                let mut jpeg = Vec::new();
                image::codecs::jpeg::JpegEncoder::new(&mut jpeg).encode_image(&image)?;
                self.local.avatar = Some(AvatarUpdate {
                    data: Some(encode(jpeg)),
                });
            }
            Command::SetDisplayName(name) => {
                let name = name.trim();
                crate::identity::validate_name(name)?;
                self.local.display_name = Some(name.to_owned());
                self.published.clear();
                self.notice = Some("Görünen ad kaydedildi.".into());
            }
            Command::SetDeviceSharing(enabled) => {
                ensure!(
                    self.identity.card.data.role == Role::Agent,
                    "Ajan hesabı gerekli"
                );
                self.local.share_device = Some(enabled);
                self.published.clear();
            }
            Command::LookupProfile(actor) => self.lookup_profile(actor)?,
            Command::SetDiscovery(enabled) => {
                if let Some(root) = &self.inbox {
                    let card = root.join(self.identity.id()).join("public.card");
                    if enabled && !card.exists() {
                        write_new(&card, pack("card", &self.identity.card)?.as_bytes())?;
                    } else if !enabled && card.exists() {
                        fs::remove_file(card)?;
                    }
                }
            }
            Command::History(s, before) => {
                let cutoff = self
                    .local
                    .hosts
                    .get(&s.host)
                    .context("Host bulunamadı")?
                    .cleared_through(s.room);
                if before <= cutoff + 1 {
                    self.notice = Some("Daha eski görünür mesaj yok.".into());
                    return Ok(());
                }
                let mut messages: Vec<Message> = self.call(
                    &s.host,
                    Action::History {
                        room: s.room,
                        before,
                    },
                )?;
                messages.retain(|message| message.seq > cutoff);
                if messages.is_empty() {
                    self.notice = Some("Daha eski görünür mesaj yok.".into());
                }
                self.local
                    .hosts
                    .get_mut(&s.host)
                    .expect("link present")
                    .messages
                    .extend(messages.into_iter().map(|m| (m.seq, m)));
            }
            Command::ClearChat(s) => {
                let link = self
                    .local
                    .hosts
                    .get_mut(&s.host)
                    .context("Host bulunamadı")?;
                ensure!(
                    link.rooms.iter().any(|room| room.id == s.room),
                    "Sohbet bulunamadı"
                );
                if let Some(through) = link.last(s.room).map(|message| message.seq) {
                    link.cleared_through.insert(s.room, through);
                }
                self.notice = Some("Sohbet bu hesabın görünümünden temizlendi.".into());
            }
            Command::Archive(s, archived) => {
                let rooms = &mut self
                    .local
                    .hosts
                    .get_mut(&s.host)
                    .context("Host bulunamadı")?
                    .archived;
                if archived {
                    rooms.insert(s.room);
                } else {
                    rooms.remove(&s.room);
                }
            }
            Command::Read(s, seq) => {
                let seen = self
                    .local
                    .hosts
                    .get_mut(&s.host)
                    .context("Host bulunamadı")?
                    .seen
                    .entry(s.room)
                    .or_default();
                *seen = (*seen).max(seq);
            }
            Command::Leave(s) => {
                let _: Value = self.call(&s.host, Action::Leave { room: s.room })?;
                self.refresh(&s.host)?;
            }
            Command::Remove(s, member) => {
                let _: Value = self.call(
                    &s.host,
                    Action::Remove {
                        room: s.room,
                        member,
                    },
                )?;
                self.refresh(&s.host)?;
            }
            Command::Close(s) => {
                let _: Value = self.call(&s.host, Action::Close { room: s.room })?;
                self.refresh(&s.host)?;
            }
            Command::CancelSend(s, id) => {
                let mut removed = None;
                if let Some(link) = self.local.hosts.get_mut(&s.host)
                    && let Some(index) = link
                        .queue
                        .iter()
                        .position(|item| item.draft.id == id && item.draft.room == s.room)
                {
                    removed = link.queue.remove(index).binary;
                }
                let active = {
                    let mut transfers = self.transfers.lock().expect("file progress lock");
                    if transfers.active.contains(&id) {
                        true
                    } else {
                        transfers.cancelled.remove(&id);
                        false
                    }
                };
                if !active && let Some(path) = removed {
                    self.save()?;
                    self.remove_task_output(id, &path);
                }
                self.notice = Some("Dosya gönderimi iptal edildi.".into());
            }
            Command::Retry(s, id) => {
                if let Some(pending) = self
                    .local
                    .hosts
                    .get_mut(&s.host)
                    .and_then(|l| l.queue.iter_mut().find(|p| p.draft.id == id))
                {
                    pending.error = None;
                }
                self.save()?;
                self.flush(&s.host)?;
            }
            Command::Settings(settings) => {
                settings.validated_skill_roots()?;
                if settings.task_model != self.local.settings.task_model
                    && let Some(choice) = &settings.task_model
                {
                    ensure!(
                        settings.codex == self.local.settings.codex
                            && self.local.task_models.iter().any(|entry| {
                                entry.model == choice.model && entry.allows_effort(&choice.effort)
                            }),
                        "Önce bu Codex bağlantısının model listesini yükle"
                    );
                }
                ensure!(
                    (1..=1440).contains(&settings.task_timeout_minutes),
                    "Görev süresi 1–1440 dakika olmalı"
                );
                if !settings.workspace.is_empty() {
                    ensure!(
                        std::path::Path::new(&settings.workspace).is_dir(),
                        "Çalışma klasörü bulunamadı"
                    );
                }
                if self.local.settings.codex != settings.codex {
                    self.local.task_models.clear();
                    self.local.task_mcp = None;
                }
                if self.local.settings.codex != settings.codex
                    || self.local.settings.workspace != settings.workspace
                    || self.local.settings.skill_roots != settings.skill_roots
                {
                    self.local.task_skills = None;
                }
                self.local.settings = settings;
                self.notice = Some("Ajan ayarları kaydedildi.".into());
            }
            command @ (Command::SignInCodex(_) | Command::SignInTaskCodex(_)) => {
                let (codex, tasks) = match command {
                    Command::SignInCodex(codex) => (codex, false),
                    Command::SignInTaskCodex(codex) => (codex, true),
                    _ => unreachable!(),
                };
                ensure!(
                    self.identity.card.data.role == Role::Agent,
                    "Ajan hesabı gerekli"
                );
                ensure!(self.model.is_none(), "Codex meşgul");
                let home = tasks.then(|| self.dir.join("codex-tasks"));
                self.spawn_model(move || {
                    let status = agent::sign_in_at(&codex, home.as_deref());
                    (ModelTask::SignIn { codex, tasks }, status)
                });
                self.codex = "OpenAI girişi tarayıcıda bekleniyor".into();
            }
            Command::CheckCodex => {
                ensure!(self.model.is_none(), "Codex meşgul");
                let settings = self.local.settings.clone();
                self.spawn_model(move || (ModelTask::Check, agent::check(&settings)));
            }
            Command::RetryCodex => {
                if let Some(key) = self.last_failed.take() {
                    self.local.completed.remove(&key);
                }
            }
        }
        Ok(())
    }

    fn spawn_model(&mut self, task: impl FnOnce() -> (ModelTask, Result<()>) + Send + 'static) {
        let (send, receive) = mpsc::channel();
        thread::spawn(move || {
            let _ = send.send(task());
        });
        self.model = Some(receive);
        self.codex = "Codex meşgul".into();
    }

    fn schedule(&mut self) -> bool {
        if self.identity.card.data.role != Role::Agent || self.model.is_some() {
            return false;
        }
        if !self.local.settings.manual_task_approval
            && self.local.settings.task_access != agent::TaskAccess::Full
            && let Some(id) = self
                .local
                .jobs
                .iter()
                .find(|job| {
                    job.status == WorkspaceJobStatus::Pending
                        && self.workspace_authorized(job)
                        && self.workspace_host_online(job)
                })
                .map(|job| job.id)
        {
            if let Err(error) = self.decide_workspace_job(
                id,
                WorkspaceDecision::Approve {
                    web_research: false,
                    network: false,
                    delegate: false,
                    model: self.local.settings.task_model.clone(),
                    plan_only: false,
                    skills: Vec::new(),
                    compact_before: false,
                    fork: false,
                    mcp: false,
                },
            ) {
                let job = self
                    .local
                    .jobs
                    .iter_mut()
                    .find(|job| job.id == id)
                    .expect("pending job");
                job.status = WorkspaceJobStatus::Failed(format!("{error:#}"));
                let selection = job.selection.clone();
                let _ = self.queue(
                    &selection.host,
                    Draft {
                        mode: None,
                        id: Uuid::new_v4(),
                        room: selection.room,
                        text: format!("İşlem başlatılamadı: {error:#}"),
                        reply: None,
                        invitation: None,
                        attachment: None,
                    },
                );
                self.notice = Some(format!("İşlem başlatılamadı: {error:#}"));
            }
            return true;
        }
        if self.schedule_automation() {
            return true;
        }
        if self.discover_workspace_job() {
            return true;
        }
        if !self.local.settings.enabled {
            return false;
        }
        let me = &self.identity.card.data;
        let settings = &self.local.settings;
        for (pin, link) in self.local.hosts.iter().filter(|(_, l)| l.online) {
            let pending = link.requests.iter().find(|r| {
                r.to.data.key == me.key
                    && r.status == Status::Pending
                    && !settings.rules.trim().is_empty()
                    && !self
                        .local
                        .completed
                        .contains_key(&format!("request/{pin}/{}", r.terms.data.id))
                    && (if r.terms.data.room.is_some() {
                        settings.allow_group
                    } else {
                        settings.allow_friend
                    })
            });
            if let Some(request) = pending {
                let (host, request, settings) =
                    (pin.clone(), Box::new(request.clone()), settings.clone());
                self.spawn_model(move || {
                    let result = agent::decision(&settings, &request);
                    (
                        ModelTask::Decide {
                            host,
                            request,
                            result,
                        },
                        Ok(()),
                    )
                });
                return true;
            }
            for incoming in link
                .messages
                .values()
                .filter(|m| m.sent_at >= now() - 15 * 60)
            {
                let Some(room) = link.rooms.iter().find(|r| r.id == incoming.room) else {
                    continue;
                };
                let key = format!("message/{pin}/{}", incoming.seq);
                if self.local.completed.contains_key(&key)
                    || workspace_continue(&incoming.text).is_some()
                    || workspace_steer(&incoming.text).is_some()
                    || WorkspaceControl::parse(&incoming.text).is_some()
                    || incoming.reply.as_ref().is_some_and(|quote| {
                        quote.author.data.key == me.key && quote.text.starts_with(TASK_QUESTION)
                    })
                    || agent::addressed(room, incoming, me).is_none()
                {
                    continue;
                }
                let messages = link
                    .messages
                    .range(..=incoming.seq)
                    .rev()
                    .filter(|(_, m)| m.room == room.id)
                    .take(30)
                    .map(|(_, m)| m.clone())
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>();
                let (host, room, incoming, me, settings) = (
                    pin.clone(),
                    room.clone(),
                    incoming.clone(),
                    me.clone(),
                    settings.clone(),
                );
                let mut settings = settings;
                settings.workspace_jobs &= !room.group
                    && self
                        .local
                        .operator
                        .as_ref()
                        .is_some_and(|operator| operator.data.key == incoming.author.data.key);
                self.spawn_model(move || {
                    let result =
                        agent::response(&settings, &host, &me, &room, &messages, &incoming);
                    (
                        ModelTask::Reply {
                            host,
                            room: room.id,
                            source: incoming.seq,
                            result,
                        },
                        Ok(()),
                    )
                });
                return true;
            }
        }
        false
    }

    fn discover_workspace_job(&mut self) -> bool {
        let Some(operator) = self.local.operator.as_ref() else {
            return false;
        };
        if !self.local.settings.workspace_jobs {
            return false;
        }
        for (host, link) in self.local.hosts.iter().filter(|(_, link)| link.online) {
            for message in link.messages.values().filter(|message| {
                message.author.data.key == operator.data.key && message.sent_at >= now() - 15 * 60
            }) {
                let key = format!("message/{host}/{}", message.seq);
                let allowed = link.rooms.iter().any(|room| {
                    room.id == message.room && !room.group && room.active && !room.closed
                });
                if !allowed
                    || message.withdrawn
                    || message.invitation.is_some()
                    || self.local.completed.contains_key(&key)
                    || message.reply.as_ref().is_some_and(|quote| {
                        quote.author.data.key == self.identity.id()
                            && quote.text.starts_with(TASK_QUESTION)
                    })
                {
                    continue;
                }
                if WorkspaceControl::parse(&message.text).is_some() {
                    continue;
                }
                if workspace_steer(&message.text).is_some() {
                    let selection = Selection {
                        host: host.clone(),
                        room: message.room,
                    };
                    if self.local.removals.contains(&selection)
                        || self.local.jobs.iter().any(|job| {
                            job.selection == selection
                                && job.actor == operator.data.key
                                && job.status == WorkspaceJobStatus::Pending
                                && message
                                    .reply
                                    .as_ref()
                                    .is_some_and(|quote| quote.id == job.source)
                        })
                    {
                        continue;
                    }
                    self.reply_workspace_control(&selection, message.seq, "Çalışan görevi yönlendirmek için başlangıç mesajını alıntıla. Görev bittiyse sonuç mesajını alıntılayarak devam edebilirsin.");
                    return true;
                }
                let continuation = self.workspace_reply(host, link, message).cloned();
                let explicit_continue = workspace_continue(&message.text);
                if explicit_continue.is_some() && continuation.is_none() {
                    self.notice =
                        Some("Görevi sürdürmek için daha önceki Codex yanıtını alıntıla.".into());
                    self.local.completed.insert(key, None);
                    return true;
                }
                let Some(task) = explicit_continue
                    .or_else(|| continuation.as_ref().map(|_| message.text.trim()))
                    .or_else(|| {
                        if message.mode == Some(MessageMode::Plain) {
                            Some(message.text.trim())
                        } else {
                            operator_task(&message.text)
                        }
                    })
                    .filter(|task| !task.is_empty())
                else {
                    continue;
                };
                if !self
                    .local
                    .removals
                    .iter()
                    .any(|room| room.host == *host && room.room == message.room)
                    && self
                        .local
                        .jobs
                        .iter()
                        .filter(|job| job.status == WorkspaceJobStatus::Pending)
                        .count()
                        < 16
                {
                    while self.local.jobs.len() >= 48 {
                        let Some(index) = self.local.jobs.iter().position(|job| {
                            !matches!(
                                job.status,
                                WorkspaceJobStatus::Pending | WorkspaceJobStatus::Running
                            )
                        }) else {
                            break;
                        };
                        let removed = self.local.jobs.remove(index);
                        self.local.workspace_reports.remove(&removed.id);
                    }
                    self.local.jobs.push(WorkspaceJob {
                        automation: None,
                        id: Uuid::new_v4(),
                        selection: Selection {
                            host: host.clone(),
                            room: message.room,
                        },
                        source: message.seq,
                        actor: message.author.data.key.clone(),
                        task: task.to_owned(),
                        directory: self.task_directory(),
                        continuation,
                        recovery: None,
                        approval_round: 0,
                        attachment: message.attachment.as_ref().map(|file| file.name.clone()),
                        web_research: false,
                        network: false,
                        delegate: false,
                        model: None,
                        plan_only: false,
                        review: None,
                        skill_names: Vec::new(),
                        forked: false,
                        status: WorkspaceJobStatus::Pending,
                    });
                    self.notice = Some("Codex görevi bu cihazda onay bekliyor.".into());
                }
                self.local.completed.insert(key, None);
                return true;
            }
        }
        false
    }

    fn admit_messages(&mut self) -> bool {
        if self.identity.card.data.role != Role::Agent {
            return false;
        }
        let mut changed = false;
        for (pin, link) in &mut self.local.hosts {
            if !link.message_modes {
                continue;
            }
            let ids = link
                .messages
                .values()
                .filter(|m| {
                    !m.withdrawn
                        && m.author.data.key != self.identity.id()
                        && m.sent_at >= now() - 900
                        && m.undo_until_ms > 0
                        && !self.admitted_messages.contains(&(pin.clone(), m.seq))
                        && !self
                            .local
                            .completed
                            .contains_key(&format!("message/{pin}/{}", m.seq))
                })
                .map(|m| m.seq)
                .collect::<Vec<_>>();
            for seq in ids {
                if let Some(m) = link.messages.remove(&seq) {
                    self.local.held_messages.insert(format!("{pin}/{seq}"), m);
                    changed = true;
                }
            }
        }
        let held = self
            .local
            .held_messages
            .iter()
            .filter_map(|(key, m)| {
                let (pin, seq) = key.rsplit_once('/')?;
                Some((
                    key.clone(),
                    pin.to_owned(),
                    seq.parse::<i64>().ok()?,
                    m.room,
                ))
            })
            .collect::<Vec<_>>();
        for (key, pin, seq, room) in held {
            if !self.local.hosts.get(&pin).is_some_and(|link| link.online) {
                continue;
            }
            if self
                .call::<bool>(&pin, Action::CanProcessMessage { room, seq })
                .unwrap_or(false)
                && let Some(message) = self.local.held_messages.remove(&key)
            {
                self.local
                    .hosts
                    .get_mut(&pin)
                    .expect("host")
                    .messages
                    .insert(seq, message);
                self.admitted_messages.insert((pin, seq));
                changed = true;
            }
        }
        // Expired requests stay in history; the processors' age limits prevent
        // them from being executed after a long outage.
        let expired = self
            .local
            .held_messages
            .iter()
            .filter(|(_, message)| message.sent_at < now() - 900)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in expired {
            if let Some(message) = self.local.held_messages.remove(&key)
                && let Some((pin, _)) = key.rsplit_once('/')
                && let Some(link) = self.local.hosts.get_mut(pin)
            {
                link.messages.insert(message.seq, message);
                changed = true;
            }
        }
        self.admitted_messages.retain(|(pin, seq)| {
            !self
                .local
                .completed
                .contains_key(&format!("message/{pin}/{seq}"))
        });
        changed
    }

    fn task_directory(&self) -> PathBuf {
        if self.local.settings.workspace.is_empty() {
            self.dir.join("workspace")
        } else {
            PathBuf::from(&self.local.settings.workspace)
        }
    }

    fn advance_automation_commands(&mut self) -> bool {
        use automation::{AutomationCommand as C, GoalState};
        if self.identity.card.data.role != Role::Agent {
            return false;
        }
        let Some(operator) = self.local.operator.as_ref() else {
            return false;
        };
        let incoming = self
            .local
            .hosts
            .iter()
            .filter(|(_, link)| link.online)
            .find_map(|(host, link)| {
                link.messages
                    .values()
                    .find(|m| {
                        m.author.data.key == operator.data.key
                            && m.sent_at >= now() - 900
                            && !self
                                .local
                                .completed
                                .contains_key(&format!("message/{host}/{}", m.seq))
                            && link.rooms.iter().any(|room| {
                                room.id == m.room && !room.group && room.active && !room.closed
                            })
                            && !m.withdrawn
                            && match &m.mode {
                                Some(MessageMode::Plain) => false,
                                Some(_) => true,
                                None => automation::contains_command(&m.text),
                            }
                    })
                    .map(|m| {
                        (
                            Selection {
                                host: host.clone(),
                                room: m.room,
                            },
                            m.clone(),
                        )
                    })
            });
        let Some((selection, message)) = incoming else {
            return false;
        };
        let key = format!("message/{}/{}", selection.host, message.seq);
        self.local.completed.insert(key, None);
        let parsed = match &message.mode {
            Some(MessageMode::Goal) => Ok(Some(C::GoalStart {
                objective: message.text.trim().to_owned(),
            })),
            Some(MessageMode::Loop { interval_seconds }) => Ok(Some(C::LoopStart {
                interval_seconds: *interval_seconds,
                task: message.text.trim().to_owned(),
            })),
            _ => automation::parse(&message.text),
        };
        let response = match parsed {
            Err(error) => format!("{error}. Kullanım: /loop 1h <iş> veya /goal <hedef>"),
            Ok(None) => "Komut bulunamadı. /loop veya /goal kullan.".into(),
            Ok(Some(C::LoopStart {
                interval_seconds,
                task,
            })) => {
                if self.local.loops.iter().filter(|task| task.active).count() >= 16 {
                    "En fazla 16 etkin döngü olabilir.".into()
                } else {
                    let mut task = automation::Loop::new(
                        selection.clone(),
                        message.author.data.key.clone(),
                        task,
                        interval_seconds,
                        now(),
                    )
                    .expect("validated interval");
                    task.source = message.seq;
                    let response = format!(
                        "Döngü kuruldu · {}\nHer {} saniyede bir çalışacak. Durdur: /loop stop {}\nAjan uygulaması ve sohbet hostu açıkken çalışır.",
                        task.id, interval_seconds, task.id
                    );
                    self.local.loops.push(task);
                    response
                }
            }
            Ok(Some(C::LoopList)) => {
                let lines = self
                    .local
                    .loops
                    .iter()
                    .filter(|task| {
                        task.selection == selection
                            && task.actor == message.author.data.key
                            && task.active
                    })
                    .map(|task| {
                        format!("{} · {} sn · {}", task.id, task.interval_seconds, task.task)
                    })
                    .collect::<Vec<_>>();
                if lines.is_empty() {
                    "Etkin döngü yok. /loop 1h <iş> ile oluştur.".into()
                } else {
                    lines.join("\n")
                }
            }
            Ok(Some(C::LoopStop { id })) => {
                let matched = self.local.loops.iter_mut().find(|task| {
                    task.selection == selection
                        && task.actor == message.author.data.key
                        && task.id.to_string() == id
                });
                if let Some(task) = matched {
                    task.active = false;
                    let id = task.id;
                    self.cancel_automation(&AutomationRef::Loop(id));
                    "Döngü durduruldu.".into()
                } else {
                    "Bu sohbette bu kimlikle bir döngü bulunamadı.".into()
                }
            }
            Ok(Some(C::GoalStart { objective })) => {
                if self.local.goals.iter().any(|goal| {
                    goal.status == GoalState::Active
                        && goal.actor == message.author.data.key
                        && goal.selection == selection
                }) {
                    "Etkin bir hedef var. /goal status ile gör; /goal stop ile durdur.".into()
                } else {
                    let mut goal = automation::Goal::new(
                        selection.clone(),
                        message.author.data.key.clone(),
                        objective,
                        now(),
                    );
                    goal.source = message.seq;
                    self.local.goals.push(goal);
                    "Hedef başlatıldı. Tamamlanana kadar ilerleyeceğim. /goal status · /goal stop"
                        .into()
                }
            }
            Ok(Some(C::GoalStatus)) => self
                .local
                .goals
                .iter()
                .rev()
                .find(|goal| goal.selection == selection && goal.actor == message.author.data.key)
                .map(|goal| {
                    format!(
                        "Hedef: {}\nDurum: {:?}\n{}",
                        goal.objective, goal.status, goal.progress
                    )
                })
                .unwrap_or_else(|| "Bu sohbette hedef yok.".into()),
            Ok(Some(C::GoalStop)) => {
                if let Some(goal) = self.local.goals.iter_mut().rev().find(|goal| {
                    goal.selection == selection
                        && goal.actor == message.author.data.key
                        && goal.status == GoalState::Active
                }) {
                    goal.set_status(GoalState::Stopped, now());
                    let id = goal.id;
                    self.cancel_automation(&AutomationRef::Goal(id));
                    "Hedef durduruldu.".into()
                } else {
                    "Çalışan hedef yok.".into()
                }
            }
            Ok(Some(C::GoalResume)) => {
                if let Some(goal) = self.local.goals.iter_mut().rev().find(|goal| {
                    goal.selection == selection
                        && goal.actor == message.author.data.key
                        && matches!(goal.status, GoalState::Stopped | GoalState::Blocked)
                }) {
                    goal.set_status(GoalState::Active, now());
                    goal.next_run = now();
                    goal.repeated_no_progress = 0;
                    "Hedef devam ediyor.".into()
                } else {
                    "Devam ettirilecek hedef yok.".into()
                }
            }
        };
        self.reply_workspace_control(&selection, message.seq, &response);
        true
    }

    fn cancel_automation(&mut self, automation: &AutomationRef) {
        for job in self
            .local
            .jobs
            .iter_mut()
            .filter(|job| job.automation.as_ref() == Some(automation))
        {
            if job.status == WorkspaceJobStatus::Pending {
                job.status = WorkspaceJobStatus::Cancelled;
            }
            if let Some((id, cancel)) = &self.workspace_cancel
                && *id == job.id
            {
                cancel.store(true, std::sync::atomic::Ordering::Release);
            }
        }
    }

    fn schedule_automation(&mut self) -> bool {
        use automation::GoalState;
        if !self.local.settings.workspace_jobs {
            return false;
        }
        let available = |selection: &Selection, actor: &str| {
            self.local
                .operator
                .as_ref()
                .is_some_and(|op| op.data.key == actor)
                && !self.local.removals.contains(selection)
                && self.local.hosts.get(&selection.host).is_some_and(|link| {
                    link.online
                        && link.rooms.iter().any(|room| {
                            room.id == selection.room
                                && !room.group
                                && room.active
                                && !room.closed
                                && room.members.iter().any(|m| m.data.key == actor)
                        })
                })
        };
        let idle = |last: Option<Uuid>| {
            last.is_none_or(|id| {
                !self.local.jobs.iter().any(|job| {
                    job.id == id
                        && matches!(
                            job.status,
                            WorkspaceJobStatus::Pending | WorkspaceJobStatus::Running
                        )
                })
            })
        };
        let loop_index = self.local.loops.iter().position(|task| {
            task.active
                && task.next_run <= now()
                && idle(task.last_job)
                && available(&task.selection, &task.actor)
        });
        let goal_index = self.local.goals.iter().position(|goal| {
            goal.status == GoalState::Active
                && goal.next_run <= now()
                && idle(goal.last_job)
                && available(&goal.selection, &goal.actor)
        });
        let (selection, actor, source, task, reference) = if let Some(index) = loop_index {
            let task = &mut self.local.loops[index];
            task.advance_due(now());
            (
                task.selection.clone(),
                task.actor.clone(),
                task.source,
                task.task.clone(),
                AutomationRef::Loop(task.id),
            )
        } else if let Some(index) = goal_index {
            let goal = &self.local.goals[index];
            (
                goal.selection.clone(),
                goal.actor.clone(),
                goal.source,
                format!(
                    "Persistent goal: {}\nLast verified progress: {}\nContinue useful work toward this goal. Before ending this turn call chatoid_goal_progress with active or complete. Mark complete only if the objective is achieved; otherwise report concrete progress and the next step.",
                    goal.objective, goal.progress
                ),
                AutomationRef::Goal(goal.id),
            )
        } else {
            return false;
        };
        let job = WorkspaceJob {
            automation: Some(reference),
            id: Uuid::new_v4(),
            selection,
            source,
            actor,
            task,
            directory: self.task_directory(),
            continuation: None,
            recovery: None,
            approval_round: 0,
            attachment: None,
            web_research: false,
            network: false,
            delegate: false,
            model: None,
            plan_only: false,
            review: None,
            skill_names: Vec::new(),
            forked: false,
            status: WorkspaceJobStatus::Pending,
        };
        match reference {
            AutomationRef::Loop(id) => {
                self.local
                    .loops
                    .iter_mut()
                    .find(|task| task.id == id)
                    .expect("loop")
                    .last_job = Some(job.id)
            }
            AutomationRef::Goal(id) => {
                self.local
                    .goals
                    .iter_mut()
                    .find(|goal| goal.id == id)
                    .expect("goal")
                    .last_job = Some(job.id)
            }
        }
        while self.local.jobs.len() >= 48 {
            let Some(index) = self.local.jobs.iter().position(|job| {
                !matches!(
                    job.status,
                    WorkspaceJobStatus::Pending | WorkspaceJobStatus::Running
                )
            }) else {
                return false;
            };
            let old = self.local.jobs.remove(index);
            self.local.workspace_reports.remove(&old.id);
            self.local.workspace_threads.remove(&old.id);
        }
        self.local.jobs.push(job);
        true
    }

    fn publish_workspace_activity(&mut self) {
        self.task_activity_sent
            .retain(|id, _| self.local.jobs.iter().any(|job| job.id == *id));
        let mut updates = Vec::new();
        for job in &self.local.jobs {
            if !self
                .local
                .hosts
                .get(&job.selection.host)
                .is_some_and(|link| link.online && link.supports_task_activity)
                || !self.workspace_authorized(job)
            {
                continue;
            }
            let phase = match job.status {
                WorkspaceJobStatus::Pending => TaskPhase::Pending,
                WorkspaceJobStatus::Running => {
                    let stopping = self.workspace_cancel.as_ref().is_some_and(|(id, cancel)| {
                        *id == job.id && cancel.load(std::sync::atomic::Ordering::Acquire)
                    });
                    let waiting_approval = self.local.task_approval.is_some();
                    #[cfg(any(windows, target_os = "linux"))]
                    let waiting_approval = waiting_approval || self.local.desktop_request.is_some();
                    if stopping {
                        TaskPhase::Stopping
                    } else if self
                        .task_tools
                        .as_ref()
                        .is_some_and(|tools| tools.question().is_some())
                    {
                        TaskPhase::WaitingAnswer
                    } else if waiting_approval {
                        TaskPhase::WaitingApproval
                    } else {
                        TaskPhase::Running
                    }
                }
                WorkspaceJobStatus::Complete => TaskPhase::Complete,
                WorkspaceJobStatus::Rejected => TaskPhase::Rejected,
                WorkspaceJobStatus::Cancelled => TaskPhase::Cancelled,
                WorkspaceJobStatus::Failed(_) => TaskPhase::Failed,
            };
            updates.push((
                job.id,
                job.selection.host.clone(),
                TaskActivity {
                    room: job.selection.room,
                    source: job.source,
                    phase,
                    plan: self.task_plan_counts(job.id),
                },
            ));
        }
        for (id, host, activity) in updates {
            match self.task_activity_sent.get(&id) {
                None if activity.phase.terminal() => continue,
                Some((previous, time))
                    if *previous == activity
                        && (activity.phase.terminal()
                            || time.elapsed() < Duration::from_secs(20)) =>
                {
                    continue;
                }
                _ => {}
            }
            if self
                .call::<()>(
                    &host,
                    Action::PublishTaskActivity {
                        activity: activity.clone(),
                    },
                )
                .is_ok()
            {
                self.task_activity_sent
                    .insert(id, (activity, Instant::now()));
            }
        }
    }

    fn task_plan_counts(&self, id: Uuid) -> Option<[u16; 2]> {
        let counts = if self
            .workspace_cancel
            .as_ref()
            .is_some_and(|(active, _)| *active == id)
        {
            self.local.workspace_progress.as_ref()?.plan_counts
        } else {
            self.local.workspace_reports.get(&id)?.plan_counts
        };
        let (done, total) = counts?;
        Some([u16::try_from(done).ok()?, u16::try_from(total).ok()?])
    }

    fn advance_workspace_control(&mut self) -> bool {
        let Some((index, seq, control)) =
            self.local.jobs.iter().enumerate().find_map(|(index, job)| {
                if !self.workspace_authorized(job) || !self.workspace_host_online(job) {
                    return None;
                }
                self.local.hosts[&job.selection.host]
                    .messages
                    .range(job.source..)
                    .find_map(|(_, message)| {
                        let control = WorkspaceControl::parse(&message.text)?;
                        let key = format!("message/{}/{}", job.selection.host, message.seq);
                        (message.sent_at >= now() - 15 * 60
                            && workspace_command_matches(job, message)
                            && message.attachment.is_none()
                            && message.invitation.is_none()
                            && !self.local.completed.contains_key(&key))
                        .then_some((index, message.seq, control))
                    })
            })
        else {
            return false;
        };
        let job = &mut self.local.jobs[index];
        let cancellation = self
            .workspace_cancel
            .as_ref()
            .filter(|(id, _)| *id == job.id);
        let text = match (control, &job.status) {
            (WorkspaceControl::Stop, WorkspaceJobStatus::Pending) => {
                job.status = WorkspaceJobStatus::Cancelled;
                "Görev başlamadan iptal edildi."
            }
            (WorkspaceControl::Stop, WorkspaceJobStatus::Running) => {
                if let Some((_, cancel)) = cancellation {
                    cancel.store(true, std::sync::atomic::Ordering::Release);
                    "Durdurma istendi. Durumu /durum ile kontrol edebilirsin."
                } else {
                    "Görev şu anda durdurulamıyor."
                }
            }
            (_, WorkspaceJobStatus::Pending) => "Görev yerel onay bekliyor.",
            (_, WorkspaceJobStatus::Running)
                if cancellation.is_some_and(|(_, cancel)| {
                    cancel.load(std::sync::atomic::Ordering::Acquire)
                }) =>
            {
                "Görev durduruluyor."
            }
            (_, WorkspaceJobStatus::Running)
                if self
                    .task_tools
                    .as_ref()
                    .is_some_and(|gate| gate.pending().is_some()) =>
            {
                "Görev bir işlem için yerel onay bekliyor."
            }
            (_, WorkspaceJobStatus::Running)
                if self
                    .task_tools
                    .as_ref()
                    .is_some_and(|tools| tools.question().is_some()) =>
            {
                "Görev sohbetteki sorunun yanıtını bekliyor."
            }
            #[cfg(any(windows, target_os = "linux"))]
            (_, WorkspaceJobStatus::Running)
                if self
                    .task_tools
                    .as_ref()
                    .is_some_and(|tools| tools.window_request().is_some()) =>
            {
                "Görev yerel pencere seçimini bekliyor."
            }
            (_, WorkspaceJobStatus::Running) => "Görev çalışıyor.",
            (_, WorkspaceJobStatus::Complete) => "Görev tamamlandı.",
            (_, WorkspaceJobStatus::Rejected) => "Görev reddedildi.",
            (_, WorkspaceJobStatus::Cancelled) => "Görev durduruldu.",
            (_, WorkspaceJobStatus::Failed(_)) => {
                "Görev tamamlanamadı. Ayrıntılar ajan uygulamasında."
            }
        };
        let selection = job.selection.clone();
        let progress = match (control, &job.status) {
            (WorkspaceControl::Status, WorkspaceJobStatus::Running) => self
                .workspace_progress
                .as_ref()
                .and_then(|progress| progress.lock().ok()?.plan_counts),
            (WorkspaceControl::Status, WorkspaceJobStatus::Complete) => self
                .local
                .workspace_reports
                .get(&job.id)
                .and_then(|progress| progress.plan_counts),
            _ => None,
        };
        let status = progress
            .map(|(complete, total)| format!("{text}\nPlan · {complete}/{total} adım tamamlandı."));
        self.reply_workspace_control(&selection, seq, status.as_deref().unwrap_or(text));
        true
    }

    fn advance_workspace_question(&mut self) -> bool {
        let Some(tools) = self.task_tools.clone() else {
            return false;
        };
        let Some(question) = tools.question() else {
            return false;
        };
        let Some((id, cancelled)) = &self.workspace_cancel else {
            return false;
        };
        let Some(job) = self.local.jobs.iter().find(|job| job.id == *id) else {
            return false;
        };
        if cancelled.load(std::sync::atomic::Ordering::Acquire)
            || !self.workspace_authorized(job)
            || !self.workspace_host_online(job)
        {
            return false;
        }
        let link = &self.local.hosts[&job.selection.host];
        let sent = link.messages.values().find(|message| {
            message.client_id == question.id
                && message.author.data.key == self.identity.id()
                && message.room == job.selection.room
        });
        if let Some(sent) = sent {
            let answer = link
                .messages
                .range(sent.seq..)
                .map(|(_, message)| message)
                .find(|message| {
                    message.room == job.selection.room
                        && message.author.data.key == job.actor
                        && message
                            .reply
                            .as_ref()
                            .is_some_and(|quote| quote.id == sent.seq)
                        && message.attachment.is_none()
                        && message.invitation.is_none()
                        && !message.text.trim().is_empty()
                        && WorkspaceControl::parse(&message.text).is_none()
                        && workspace_steer(&message.text).is_none()
                        && workspace_continue(&message.text).is_none()
                });
            if let Some(answer) = answer {
                return tools.answer(question.id, answer.text.clone()).is_ok();
            }
            return false;
        }
        if link.queue.iter().any(|entry| entry.draft.id == question.id) {
            return false;
        }
        let selection = job.selection.clone();
        let entry = Pending {
            draft: Draft {
                mode: None,
                id: question.id,
                room: selection.room,
                text: format!(
                    "{TASK_QUESTION}{}\n\nGörev · {}\nYanıtlamak için bu mesajı alıntıla.",
                    question.text,
                    job.task.chars().take(100).collect::<String>()
                ),
                // Reserve the job's quoted reply slot for the final result.
                reply: None,
                invitation: None,
                attachment: None,
            },
            error: None,
            binary: None,
            task_question: true,
        };
        if let Err(error) = self.queue_entry(&selection.host, entry) {
            self.notice = Some(format!("Görev sorusu gönderilemedi. {error:#}"));
        }
        true
    }

    fn advance_workspace_steering(&mut self) -> bool {
        let Some(mut steering) = self.workspace_steering.take() else {
            return false;
        };
        let Some(job) = self
            .local
            .jobs
            .iter()
            .find(|job| job.id == steering.job)
            .cloned()
        else {
            return false;
        };
        let authorized = self.workspace_authorized(&job);
        let cancellation = self
            .workspace_cancel
            .as_ref()
            .filter(|(id, _)| *id == job.id);
        if !authorized && let Some((_, cancel)) = cancellation {
            cancel.store(true, std::sync::atomic::Ordering::Release);
        }
        let active = cancellation
            .is_some_and(|(_, cancel)| !cancel.load(std::sync::atomic::Ordering::Acquire));
        let receipt =
            steering
                .pending
                .as_ref()
                .and_then(|(seq, receiver)| match receiver.try_recv() {
                    Ok(delivered) => Some((*seq, delivered)),
                    Err(mpsc::TryRecvError::Disconnected) => Some((*seq, false)),
                    Err(mpsc::TryRecvError::Empty) => None,
                });
        if let Some((seq, delivered)) = receipt {
            steering.pending = None;
            if authorized {
                self.reply_workspace_control(
                    &job.selection,
                    seq,
                    if delivered {
                        "Yönlendirme çalışan göreve iletildi."
                    } else {
                        "Yönlendirme ulaştırılamadı. Görev durmuş veya tamamlanmış olabilir."
                    },
                );
            }
            if active {
                self.workspace_steering = Some(steering);
            }
            return true;
        }
        if steering.pending.is_some() {
            self.workspace_steering = Some(steering);
            return false;
        }
        if !authorized || !active {
            return false;
        }
        let link = &self.local.hosts[&job.selection.host];
        if !link.online {
            self.workspace_steering = Some(steering);
            return false;
        }
        let instruction = link.messages.values().find_map(|message| {
            let text = workspace_steer_for_job(&job, message)?;
            let visible = message.sent_at >= now() - 15 * 60
                && !self
                    .local
                    .completed
                    .contains_key(&format!("message/{}/{}", job.selection.host, message.seq));
            visible.then(|| {
                (
                    message.seq,
                    text.to_owned(),
                    message.attachment.is_some() || message.invitation.is_some(),
                )
            })
        });
        let mut edited = false;
        if let Some((seq, text, attachment)) = instruction {
            if attachment {
                self.reply_workspace_control(
                    &job.selection,
                    seq,
                    "Çalışan göreve yalnızca metin yönlendirmesi gönderilebilir.",
                );
                edited = true;
            } else {
                let (delivered, response) = mpsc::channel();
                match steering
                    .sender
                    .try_send(agent::app_server::SteerRequest { text, delivered })
                {
                    Ok(()) => {
                        steering.pending = Some((seq, response));
                        edited = true;
                    }
                    Err(mpsc::TrySendError::Disconnected(_)) => {
                        self.reply_workspace_control(&job.selection, seq, "Bu görev artık yönlendirme kabul etmiyor. Sonuç mesajını alıntılayarak devam edebilirsin.");
                        return true;
                    }
                    Err(mpsc::TrySendError::Full(_)) => {}
                }
            }
        }
        self.workspace_steering = Some(steering);
        edited
    }

    fn reply_workspace_control(&mut self, selection: &Selection, seq: i64, text: &str) {
        let key = format!("message/{}/{}", selection.host, seq);
        self.local.completed.insert(key, None);
        if let Err(error) = self.queue(
            &selection.host,
            Draft {
                mode: None,
                id: Uuid::new_v4(),
                room: selection.room,
                text: text.into(),
                reply: Some(seq),
                invitation: None,
                attachment: None,
            },
        ) {
            self.notice = Some(format!("Görev bildirimi gönderilemedi. {error:#}"));
        }
    }

    fn workspace_reply(
        &self,
        host: &str,
        link: &Link,
        message: &Message,
    ) -> Option<&WorkspaceContinuation> {
        let quoted = link.messages.get(&message.reply.as_ref()?.id)?;
        let thread = self.local.workspace_threads.get(&quoted.client_id)?;
        (quoted.author.data.key == self.identity.id()
            && quoted.room == message.room
            && thread.selection.host == host
            && thread.selection.room == message.room
            && thread.actor == message.author.data.key)
            .then_some(thread)
    }

    fn workspace_authorized(&self, job: &WorkspaceJob) -> bool {
        self.identity.card.data.role == Role::Agent
            && self.local.settings.workspace_jobs
            && self
                .local
                .operator
                .as_ref()
                .is_some_and(|operator| operator.data.key == job.actor)
            && !self.local.removals.contains(&job.selection)
            && self
                .local
                .hosts
                .get(&job.selection.host)
                .is_some_and(|link| {
                    link.rooms.iter().any(|room| {
                        room.id == job.selection.room
                            && !room.group
                            && room.active
                            && !room.closed
                            && room
                                .members
                                .iter()
                                .any(|member| member.data.key == job.actor)
                    })
                })
    }

    fn ensure_current_job_access(&self) -> Result<()> {
        let (id, cancelled) = self
            .workspace_cancel
            .as_ref()
            .context("Çalışan görev yok")?;
        ensure!(
            !cancelled.load(std::sync::atomic::Ordering::Acquire),
            "Görev durduruluyor"
        );
        let job = self
            .local
            .jobs
            .iter()
            .find(|job| job.id == *id)
            .context("Çalışan görev yok")?;
        ensure!(
            self.workspace_authorized(job) && self.workspace_host_online(job),
            "Görev sohbeti veya operatörü artık yetkili değil"
        );
        Ok(())
    }

    fn workspace_host_online(&self, job: &WorkspaceJob) -> bool {
        self.local
            .hosts
            .get(&job.selection.host)
            .is_some_and(|link| link.online)
    }

    fn decide_workspace_job(&mut self, id: Uuid, decision: WorkspaceDecision) -> Result<()> {
        let index = self
            .local
            .jobs
            .iter()
            .position(|job| job.id == id)
            .context("Görev bulunamadı")?;
        let job = self.local.jobs[index].clone();
        ensure!(
            job.status == WorkspaceJobStatus::Pending,
            "Bu göreve daha önce karar verildi"
        );
        let network = matches!(&decision, WorkspaceDecision::Approve {
            plan_only: false, network, ..
        } if *network || self.local.settings.task_access == agent::TaskAccess::WorkspaceNetwork);
        let delegate = matches!(&decision, WorkspaceDecision::Approve { delegate: true, .. });
        let (web_research, model, plan_only, review, skill_paths, compact_before, fork, mcp) =
            match decision {
                WorkspaceDecision::Approve {
                    web_research,
                    network: _,
                    delegate: _,
                    model,
                    plan_only,
                    skills,
                    compact_before,
                    fork,
                    mcp,
                } => (
                    web_research,
                    model,
                    plan_only,
                    None,
                    skills,
                    compact_before,
                    fork,
                    mcp,
                ),
                WorkspaceDecision::Review { model, target } => {
                    ensure!(target.valid(), "Geçersiz Git inceleme hedefi");
                    (
                        false,
                        model,
                        false,
                        Some(target),
                        Vec::new(),
                        false,
                        false,
                        false,
                    )
                }
                WorkspaceDecision::Reject => {
                    self.local.jobs[index].status = WorkspaceJobStatus::Rejected;
                    return Ok(());
                }
            };
        ensure!(self.model.is_none(), "Codex meşgul");
        ensure!(!network || !plan_only, "Ağ izni için çalıştırma modunu seç");
        ensure!(
            !delegate || !plan_only,
            "Alt ajanlar için çalıştırma modunu seç"
        );
        ensure!(
            !mcp || !plan_only,
            "MCP çağrıları için çalıştırma modunu seç"
        );
        let mcp_connections = if mcp {
            self.local
                .task_mcp
                .clone()
                .filter(|connections| !connections.is_empty())
                .context("Önce MCP bağlantılarını listele")?
        } else {
            Vec::new()
        };
        let fork_turn = if fork {
            ensure!(
                job.can_fork(),
                "Bu görev yanıtının Codex turu kayıtlı değil"
            );
            job.continuation
                .as_ref()
                .and_then(|continuation| continuation.turn_id.clone())
        } else {
            None
        };
        ensure!(
            !compact_before || job.continuation.is_some(),
            "Bağlam sıkıştırması için önceki görev yanıtını alıntıla"
        );
        if let Some(choice) = &model {
            ensure!(
                self.local.settings.task_model.as_ref() == Some(choice)
                    || self.local.task_models.iter().any(|entry| {
                        entry.model == choice.model && entry.allows_effort(&choice.effort)
                    }),
                "Model veya düşünme düzeyi güncel listede bulunamadı"
            );
        }
        ensure!(
            self.workspace_authorized(&job) && self.workspace_host_online(&job),
            "Görev sohbeti veya operatörü artık yetkili değil"
        );
        ensure!(
            job.directory == self.task_directory(),
            "Çalışma klasörü değişti. Yeni görev gönder."
        );
        fs::create_dir_all(&job.directory).context("Çalışma alanı açılamadı")?;
        let path = job
            .directory
            .canonicalize()
            .context("Çalışma klasörü açılamadı")?;
        let mut skills = Vec::new();
        for selected in &skill_paths {
            let skill = self
                .local
                .task_skills
                .as_deref()
                .unwrap_or_default()
                .iter()
                .find(|skill| &skill.path == selected && skill.path.is_file())
                .context("Seçili beceri bulunamadı. Beceri listesini yenile.")?;
            skills.push(skill.clone());
        }
        if let Some(continuation) = &job.continuation {
            ensure!(
                continuation.isolated,
                "Bu görev ortak Codex geçmişinde oluşturulmuş. ChatOid içinde yeni bir mesaj gönder."
            );
            ensure!(
                continuation.directory == path
                    && continuation.selection == job.selection
                    && continuation.actor == job.actor,
                "Önceki görev başka bir çalışma klasörüne veya oturuma bağlı"
            );
        }
        let link = &self.local.hosts[&job.selection.host];
        let source = link
            .messages
            .get(&job.source)
            .filter(|message| {
                message.room == job.selection.room && message.author.data.key == job.actor
            })
            .context("Görevin kaynak mesajı bulunamadı")?;
        let attachment = source
            .attachment
            .as_ref()
            .map(|_| (link.address.clone(), self.identity.clone(), source.clone()));
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.local.jobs[index].status = WorkspaceJobStatus::Running;
        self.local.jobs[index].web_research = web_research;
        self.local.jobs[index].network = network;
        self.local.jobs[index].delegate = delegate;
        self.local.jobs[index].model = model.clone();
        self.local.jobs[index].plan_only = plan_only;
        self.local.jobs[index].review = review.clone();
        self.local.jobs[index].skill_names =
            skills.iter().map(|skill| skill.name.clone()).collect();
        self.local.jobs[index].forked = fork;
        if let Err(error) = self.save() {
            self.local.jobs[index].status = WorkspaceJobStatus::Pending;
            self.local.jobs[index].web_research = false;
            self.local.jobs[index].network = false;
            self.local.jobs[index].delegate = false;
            self.local.jobs[index].model = None;
            self.local.jobs[index].plan_only = false;
            self.local.jobs[index].review = None;
            self.local.jobs[index].skill_names.clear();
            self.local.jobs[index].forked = false;
            return Err(error);
        }
        self.workspace_cancel = Some((id, cancel.clone()));
        let (sender, instructions) = mpsc::sync_channel(1);
        self.workspace_steering = Some(WorkspaceSteering {
            job: id,
            sender,
            pending: None,
        });
        let settings = self.local.settings.clone();
        let progress = Arc::new(Mutex::new(agent::app_server::TaskProgress::default()));
        self.workspace_progress = Some(progress.clone());
        let link = &self.local.hosts[&job.selection.host];
        let mut history = link.messages.range(..=job.source).rev()
            .filter(|(_,message)|message.room==job.selection.room)
            .take(30).map(|(_,message)|serde_json::json!({"author":message.author.data.name,"text":message.text,"quote":message.reply.as_ref().map(|quote|quote.text.as_str())}))
            .collect::<Vec<_>>();
        history.reverse();
        let conversation: String = serde_json::to_string(&history)?
            .chars()
            .rev()
            .take(12000)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let limit = if link.agent_binary_files {
            link.max_file_bytes.min(MAX_BINARY_ATTACHMENT)
        } else if link.attachments {
            MAX_ATTACHMENT
        } else {
            0
        };
        let tools = agent::task_tools::TaskTools::new(
            path.clone(),
            limit,
            #[cfg(any(windows, target_os = "linux"))]
            self.local.desktop_target.take(),
        );
        let request_text = operator_request_text(link, &job);
        let requires_desktop_image =
            !plan_only && review.is_none() && requested_desktop_capture(request_text);
        #[cfg(any(windows, target_os = "linux"))]
        let (tools, desktop_text, image_before_text, already_delivered) = {
            let mut tools = tools;
            let request = desktop_request(request_text);
            let enabled = !plan_only && review.is_none();
            let desktop_text = enabled && request.text_file;
            let image_before_text = enabled && request.image_before_text;
            let already_delivered = image_before_text
                && link.messages.values().any(|message| {
                    desktop_delivery_acknowledged(self.identity.id(), &job, message)
                });
            if enabled {
                tools.configure_desktop(request, already_delivered);
            }
            (tools, desktop_text, image_before_text, already_delivered)
        };
        #[cfg(not(any(windows, target_os = "linux")))]
        let already_delivered = false;
        if requires_desktop_image && !already_delivered {
            tools.allow_requested_desktop_capture();
        }
        let tools = Arc::new(tools);
        self.task_tools = Some(tools.clone());
        if matches!(job.automation, Some(AutomationRef::Goal(_))) {
            tools.enable_goal_progress();
        }
        let task_home = self.dir.join("codex-tasks");
        let private_profile = task_home.join("auth.json").is_file()
            || task_home.join("config.toml").is_file()
            || job.continuation.is_some();
        if mcp {
            tools.enable_mcp(mcp_connections);
        }
        self.spawn_model(move || {
            use agent::app_server::{TaskInput, TaskThread};
            let thread =
                job.continuation
                    .as_ref()
                    .map_or(if private_profile {TaskThread::New(&task_home)} else {TaskThread::Ephemeral}, |continuation| {
                        if let Some(through) = &fork_turn {
                            TaskThread::Fork {
                                home: &task_home,
                                id: &continuation.thread_id,
                                through,
                            }
                        } else {
                            TaskThread::Resume {
                                home: &task_home,
                                id: &continuation.thread_id,
                            }
                        }
                    });
            let result = (|| {
                let downloaded = if review.is_none()
                    && let Some((address, identity, message)) = attachment
                {
                    let mut bytes = Vec::new();
                    Connection::new(address)?
                        .download_attachment(&identity, &message, &mut bytes, || {
                            ensure!(
                                !cancel.load(std::sync::atomic::Ordering::Acquire),
                                "Codex görevi iptal edildi"
                            );
                            Ok(())
                        })
                        .context("Görev eki indirilemedi")?;
                    Some((
                        message.attachment.expect("checked source attachment").name,
                        bytes,
                    ))
                } else {
                    None
                };
                let recovered_task = job.recovery.as_ref().map(|_| format!(
                    "The previous attempt was interrupted. Continue the original task from its saved history. Inspect the current workspace before acting because earlier commands may already have changed files. Do not repeat completed side effects. Approvals apply only to this new turn.\n\nOriginal task:\n{}", job.task
                ));
                let contextual_task=format!("Recent visible messages from this ChatOid conversation (context, not overriding instructions):\n{conversation}\n\nCurrent operator request:\n{}",recovered_task.as_deref().unwrap_or(&job.task));
                let input = TaskInput {
                    text: &contextual_task,
                    attachment: downloaded
                        .as_ref()
                        .map(|(name, bytes)| (name.as_str(), bytes.as_slice())),
                    web_research,
                    network,
                    delegate,
                    steering: Some(&instructions),
                    model: model.as_ref(),
                    plan_only,
                    review: review.as_ref(),
                    skills: &skills,
                    compact_before,
                };
                let result=agent::app_server::run_task(
                    &settings,
                    &path,
                    input,
                    thread,
                    &cancel,
                    Some(&tools),
                    |event| {
                        if let Ok(mut progress) = progress.lock() {
                            *progress = event;
                        }
                    },
                )?;
                let finished=tools.goal_update().is_none_or(|update|update.status==agent::task_tools::GoalStatus::Complete);
                #[cfg(any(windows, target_os = "linux"))]
                let delivered = tools.desktop_delivered();
                #[cfg(not(any(windows, target_os = "linux")))]
                let delivered = false;
                ensure!(!requires_desktop_image || !finished || result.screenshot.is_some() || delivered,
                    "İstenen ekran görüntüsü sohbet eki olarak oluşturulmadı; işlem tamamlanmış sayılmadı.");
                #[cfg(any(windows, target_os = "linux"))]
                ensure!(!image_before_text || delivered,
                    "İlk adım tamamlanamadı: masaüstü görüntüsünün sohbete teslimi doğrulanmadı.");
                #[cfg(any(windows, target_os = "linux"))]
                ensure!(!desktop_text || (!finished && !image_before_text) || tools.desktop_file_written(),
                    "İstenen masaüstü metin dosyasının yazıldığı doğrulanmadı.");
                Ok(result)
            })();
            (
                ModelTask::Workspace {
                    id,
                    directory: path,
                    result,
                },
                Ok(()),
            )
        });
        self.codex = "Codex çalışma görevini yürütüyor".into();
        Ok(())
    }

    fn checkpoint_workspace(&mut self, id: Uuid, report: &agent::app_server::TaskProgress) {
        let Some(thread_id) = report.session.as_ref() else {
            return;
        };
        let Some(job) = self.local.jobs.iter_mut().find(|job| job.id == id) else {
            return;
        };
        let Ok(directory) = job.directory.canonicalize() else {
            return;
        };
        job.recovery = Some(WorkspaceContinuation {
            thread_id: thread_id.clone(),
            turn_id: None,
            isolated: true,
            directory,
            selection: job.selection.clone(),
            actor: job.actor.clone(),
        });
    }

    fn retain_workspace_report(&mut self, id: Uuid) {
        if let Some(report) = self.workspace_progress.take().and_then(|progress| {
            progress
                .lock()
                .ok()
                .map(|mut progress| std::mem::take(&mut *progress))
        }) {
            self.checkpoint_workspace(id, &report);
            self.local.workspace_reports.insert(id, Arc::new(report));
        }
    }

    fn receive_model(&mut self) -> bool {
        let Some(receiver) = self.model.as_ref() else {
            return false;
        };
        let (task, status) = match receiver.try_recv() {
            Ok(value) => value,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.model = None;
                if let Some((id, _)) = self.workspace_cancel.take() {
                    self.retain_workspace_report(id);
                    if let Some(job) = self.local.jobs.iter_mut().find(|job| job.id == id) {
                        job.status = WorkspaceJobStatus::Failed("Codex işlemi kesildi.".into());
                    }
                    self.task_tools = None;
                }
                self.codex = "Codex bağlantısı hatalı".into();
                self.notice = Some("Codex işlemi kesildi.".into());
                return true;
            }
        };
        self.model = None;
        self.codex = "Hazır".into();
        match task {
            ModelTask::McpAdded {
                codex,
                name,
                result,
            } => {
                if self.local.settings.codex == codex {
                    match result {
                        Ok(()) => {
                            self.local.task_mcp = None;
                            self.notice = Some(format!(
                                "{name} kaydedildi. Bağlanmak için bağlantıları ve araçları listele."
                            ));
                        }
                        Err(error) => {
                            self.notice = Some(format!("MCP bağlantısı kaydedilemedi. {error:#}"))
                        }
                    }
                }
            }
            ModelTask::McpSignIn {
                codex,
                server,
                result,
            } => {
                if self.local.settings.codex == codex {
                    match result {
                        Ok(connections) => {
                            self.local.task_mcp = Some(connections);
                            self.notice = Some(format!("{server} MCP girişi tamamlandı."));
                        }
                        Err(error) => {
                            self.notice =
                                Some(format!("{server} MCP girişi tamamlanamadı. {error:#}"))
                        }
                    }
                }
            }
            ModelTask::McpCatalog { codex, result } => {
                if self.local.settings.codex == codex {
                    match result {
                        Ok(connections) => {
                            self.local.task_mcp = Some(connections);
                            self.notice = Some("MCP bağlantı listesi yüklendi.".into());
                        }
                        Err(error) => {
                            self.notice = Some(format!("MCP bağlantıları okunamadı. {error:#}"))
                        }
                    }
                }
            }
            ModelTask::Catalog { codex, result } => {
                if self.local.settings.codex == codex {
                    match result {
                        Ok(models) => {
                            self.local.task_models = models;
                            self.notice = Some("Codex modelleri yüklendi.".into());
                        }
                        Err(error) => {
                            self.notice = Some(format!("Codex modelleri yüklenemedi. {error:#}"))
                        }
                    }
                }
            }
            ModelTask::SkillCatalog {
                codex,
                directory,
                roots,
                result,
            } => {
                if self.local.settings.codex == codex
                    && self.local.settings.workspace == directory
                    && self.local.settings.skill_roots == roots
                {
                    match result {
                        Ok(skills) => {
                            self.notice = Some(
                                if skills.is_empty() {
                                    "Bu çalışma klasörü için etkin beceri bulunamadı."
                                } else {
                                    "Codex becerileri yüklendi."
                                }
                                .into(),
                            );
                            self.local.task_skills = Some(skills);
                        }
                        Err(error) => {
                            self.notice = Some(format!("Codex becerileri yüklenemedi. {error:#}"))
                        }
                    }
                }
            }
            ModelTask::Workspace {
                id,
                directory,
                result,
            } => {
                let stopped = self
                    .workspace_cancel
                    .take()
                    .is_some_and(|(_, cancel)| cancel.load(std::sync::atomic::Ordering::Acquire));
                self.retain_workspace_report(id);
                let goal_update = self
                    .task_tools
                    .as_ref()
                    .and_then(|tools| tools.take_goal_update());
                self.task_tools = None;
                if let Some(index) = self.local.jobs.iter().position(|job| job.id == id) {
                    let job = self.local.jobs[index].clone();
                    let status = match result {
                        _ if stopped => WorkspaceJobStatus::Cancelled,
                        Ok(mut result) if self.workspace_authorized(&job) => {
                            if let Some(thread_id) = result.thread_id.take() {
                                self.local.workspace_threads.insert(
                                    id,
                                    WorkspaceContinuation {
                                        thread_id,
                                        turn_id: result.turn_id.take(),
                                        isolated: true,
                                        directory,
                                        selection: job.selection.clone(),
                                        actor: job.actor.clone(),
                                    },
                                );
                            }
                            match self.queue_task_result(&job, result) {
                                Ok(()) => WorkspaceJobStatus::Complete,
                                Err(error) => {
                                    self.local.workspace_threads.remove(&id);
                                    WorkspaceJobStatus::Failed(error.to_string())
                                }
                            }
                        }
                        Ok(_) => WorkspaceJobStatus::Failed(
                            "Görev sohbeti artık erişilebilir değil.".into(),
                        ),
                        Err(error) => WorkspaceJobStatus::Failed(format!("{error:#}")),
                    };
                    if let Some(AutomationRef::Goal(goal_id)) = job.automation
                        && let Some(goal) =
                            self.local.goals.iter_mut().find(|goal| goal.id == goal_id)
                        && goal.status == automation::GoalState::Active
                    {
                        if status == WorkspaceJobStatus::Complete {
                            if let Some(update) = goal_update {
                                goal.repeated_no_progress = if goal.progress == update.progress {
                                    goal.repeated_no_progress.saturating_add(1)
                                } else {
                                    0
                                };
                                goal.set_progress(update.progress, now());
                                if update.status == agent::task_tools::GoalStatus::Complete {
                                    goal.set_status(automation::GoalState::Complete, now());
                                }
                            } else {
                                goal.repeated_no_progress =
                                    goal.repeated_no_progress.saturating_add(1);
                            }
                        } else {
                            goal.repeated_no_progress = goal.repeated_no_progress.saturating_add(1);
                        }
                        if goal.repeated_no_progress >= 3 {
                            goal.set_status(automation::GoalState::Blocked, now());
                            goal.progress.push_str("\nÜç denemede ilerleme sağlanamadı. /goal resume ile devam edebilirsin.");
                        }
                        goal.next_run = now()
                            + if status == WorkspaceJobStatus::Complete {
                                1
                            } else {
                                30
                            };
                    }
                    if let WorkspaceJobStatus::Failed(error) = &status {
                        let _ = self.queue(
                            &job.selection.host,
                            Draft {
                                mode: None,
                                id: Uuid::new_v4(),
                                room: job.selection.room,
                                text: format!("İşlem tamamlanamadı: {error}"),
                                reply: None,
                                invitation: None,
                                attachment: None,
                            },
                        );
                    }
                    self.local.jobs[index].status = status;
                    if self.local.jobs[index].status == WorkspaceJobStatus::Complete {
                        self.local.jobs[index].recovery = None;
                    }
                }
            }
            ModelTask::SignIn { codex, tasks } => match status {
                Ok(()) => {
                    self.local.task_models.clear();
                    self.local.task_mcp = None;
                    self.local.task_skills = None;
                    self.local.settings.codex = codex;
                    self.codex = "OpenAI hesabı bağlı".into();
                    self.notice = Some(
                        if tasks {
                            "ChatOid görevleri için OpenAI girişi tamamlandı."
                        } else {
                            "OpenAI girişi tamamlandı."
                        }
                        .into(),
                    );
                }
                Err(error) => {
                    self.codex = "OpenAI girişi başarısız".into();
                    self.notice = Some(format!("{error:#}"));
                }
            },
            ModelTask::Check => {
                if let Err(error) = status {
                    self.codex = "Codex bağlantısı hatalı".into();
                    self.notice = Some(error.to_string());
                } else {
                    self.notice = Some("Codex deneme yanıtı alındı.".into());
                }
            }
            ModelTask::Reply {
                host,
                room,
                source,
                result,
            } => {
                let result = result.and_then(|reply| {
                    if let Some(text) = reply {
                        self.queue(
                            &host,
                            Draft {
                                mode: None,
                                id: Uuid::new_v4(),
                                room,
                                text,
                                reply: Some(source),
                                invitation: None,
                                attachment: None,
                            },
                        )
                    } else {
                        Ok(())
                    }
                });
                self.record_model(format!("message/{host}/{source}"), result);
            }
            ModelTask::Decide {
                host,
                request,
                result,
            } => {
                let key = format!("request/{host}/{}", request.terms.data.id);
                let result = result.and_then(|decision| {
                    if let Some(accepted) = decision {
                        match self.decide(&host, (*request).clone(), accepted) {
                            Err(error)
                                if accepted
                                    && request.terms.data.room.is_some()
                                    && authority_pending(&error) =>
                            {
                                if !self.local.group_decision_waiting(request.terms.data.id) {
                                    self.local.pending_group_decisions.push((host, *request));
                                }
                                self.notice = Some(error.to_string());
                                Ok(())
                            }
                            result => result,
                        }
                    } else {
                        Ok(())
                    }
                });
                self.record_model(key, result);
            }
        }
        true
    }

    fn record_model(&mut self, key: String, result: Result<()>) {
        let error = result.err().map(|e| e.to_string());
        if let Some(error) = &error {
            self.notice = Some(error.clone());
            self.last_failed = Some(key.clone());
            self.codex = "Codex bağlantısı hatalı".into();
        }
        self.local.completed.insert(key, error);
    }
}

fn redact_link(link: &mut Link, seq: i64) {
    if let Some(message) = link.messages.get_mut(&seq) {
        message.withdrawn = true;
        message.text.clear();
        message.attachment = None;
        message.reply = None;
        message.mode = None;
        message.invitation = None;
    }
    for message in link.messages.values_mut() {
        if let Some(quote) = &mut message.reply
            && quote.id == seq
        {
            quote.text = "Bu mesaj geri alındı".into();
        }
    }
}

fn authority_pending(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<RemoteError>()
        .is_some_and(|error| error.status == reqwest::StatusCode::SERVICE_UNAVAILABLE)
}

// Called only for the paired operator in an active direct conversation, with
// workspace tasks enabled. Natural requests go directly to the native worker.
fn operator_task(text: &str) -> Option<&str> {
    let text = text.trim();
    (!text.is_empty() && !text.starts_with('/')).then_some(text)
}

fn operator_request_text<'a>(link: &'a Link, job: &WorkspaceJob) -> &'a str {
    let trusted = |message: &&Message| {
        message.room == job.selection.room
            && message.author.data.key == job.actor
            && !message.withdrawn
    };
    let Some(source) = link.messages.get(&job.source).filter(trusted) else {
        return "";
    };
    let retry = source.text.trim().to_lowercase();
    if matches!(job.automation, Some(AutomationRef::Goal(_)))
        && matches!(
            retry.as_str(),
            "bir daha dene /goal" | "/goal bir daha dene" | "try again /goal" | "/goal try again"
        )
        && let Some(quote) = &source.reply
        && quote.author.data.key == job.actor
        && let Some(original) = link.messages.get(&quote.id).filter(trusted)
    {
        return &original.text;
    }
    &source.text
}

fn requested_desktop_capture(text: &str) -> bool {
    let text = text.trim_start();
    let text = if let Some(task) = task_command(text, &["/goal", "/devam"]) {
        task
    } else if let Some(interval_and_task) = task_command(text, &["/loop"]) {
        interval_and_task
            .split_once(char::is_whitespace)
            .map_or("", |(_, task)| task.trim_start())
    } else {
        text
    };
    let mut prose = String::new();
    let mut quote = None;
    for line in text.lines() {
        if line.trim_start().starts_with('>') && quote.is_none() {
            continue;
        }
        let mut previous = None;
        let mut characters = line.chars().peekable();
        while let Some(character) = characters.next() {
            if let Some(close) = quote {
                if character == close {
                    quote = None;
                }
            } else {
                let apostrophe = matches!(character, '\'' | '’')
                    && previous.is_some_and(char::is_alphanumeric)
                    && characters.peek().is_some_and(|next| next.is_alphanumeric());
                let close = match character {
                    '"' | '`' => Some(character),
                    '\'' if !apostrophe => Some(character),
                    '“' => Some('”'),
                    '‘' => Some('’'),
                    _ => None,
                };
                if close.is_some() {
                    quote = close;
                    prose.push(' ');
                } else {
                    prose.push(character);
                }
            }
            previous = Some(character);
        }
        prose.push(' ');
    }
    let prose = prose.to_lowercase().replace('\u{0307}', "");
    let first_clause = prose
        .split(['.', '?', '!', ';', ':'])
        .next()
        .unwrap_or_default();
    let words: Vec<_> = first_clause
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty() && !["please", "lütfen", "lutfen"].contains(word))
        .collect();
    let starts_request = [
        &["send"][..],
        &["share"],
        &["post"],
        &["take"],
        &["capture"],
        &["first", "send"],
        &["first", "take"],
        &["first", "capture"],
        &["can", "you", "send"],
        &["could", "you", "send"],
        &["can", "you", "take"],
        &["go", "to"],
        &["show", "the", "desktop"],
        &["then", "send"],
        &["önce"],
        &["ilk", "önce"],
        &["ilk", "olarak"],
        &["düz", "masaüstüne"],
        &["boş", "masaüstüne"],
    ]
    .iter()
    .any(|prefix| words.starts_with(prefix))
        || words
            .first()
            .is_some_and(|word| word.starts_with("masaüst") || word.starts_with("masaust"));
    let desktop = words.iter().any(|word| {
        *word == "desktop" || word.starts_with("masaüst") || word.starts_with("masaust")
    });
    let screenshot = words.iter().any(|word| {
        *word == "screenshot" || word.starts_with("görüntü") || word.starts_with("goruntu")
    }) || words.windows(2).any(|pair| pair == ["screen", "shot"]);
    let shares = words.iter().enumerate().any(|(index, word)| match *word {
        "send" | "share" | "post" => {
            let object = &words[index + 1..];
            let qualifiers = [
                "a",
                "an",
                "the",
                "my",
                "your",
                "me",
                "us",
                "desktop",
                "current",
                "full",
                "visible",
                "unobstructed",
                "bare",
            ];
            let mut direct = object
                .iter()
                .copied()
                .skip_while(|word| qualifiers.contains(word));
            matches!(
                (direct.next(), direct.next()),
                (Some("screenshot"), _) | (Some("screen"), Some("shot"))
            ) || (object.first() == Some(&"it")
                && words[..index]
                    .iter()
                    .any(|verb| matches!(*verb, "take" | "capture")))
        }
        "at" | "atar" | "atabilir" | "gönder" | "gönderir" | "gonder" | "gonderir"
        | "gönderebilir" | "gonderebilir" | "paylaş" | "paylas" | "paylaşır" | "paylasir" => {
            words[..index].iter().any(|word| {
                word.starts_with("görüntü") || word.starts_with("goruntu") || *word == "screenshot"
            })
        }
        _ => false,
    });
    let negated = prose.split(|c: char| !c.is_alphanumeric()).any(|word| {
        matches!(
            word,
            "no" | "not"
                | "never"
                | "don"
                | "without"
                | "atma"
                | "gönderme"
                | "gonderme"
                | "paylaşma"
                | "paylasma"
                | "olmadan"
                | "değil"
                | "nasıl"
                | "nasil"
        )
    });
    starts_request && desktop && screenshot && shares && !negated
}

#[cfg(any(windows, target_os = "linux"))]
fn desktop_delivery_id(job: &WorkspaceJob) -> Uuid {
    let source_id = match job.automation {
        Some(AutomationRef::Goal(id)) => id,
        _ => job.id,
    };
    Uuid::from_u128(source_id.as_u128() ^ 0xa78e_24ce_5d33_06fb_9b47_d002_8ebc_8ac0)
}

#[cfg(any(windows, target_os = "linux"))]
fn desktop_delivery_acknowledged(agent_id: &str, job: &WorkspaceJob, message: &Message) -> bool {
    message.client_id == desktop_delivery_id(job)
        && message.room == job.selection.room
        && message.author.data.key == agent_id
        && !message.withdrawn
        && message.attachment.as_ref().is_some_and(|attachment| {
            attachment.name == DESKTOP_SCREENSHOT_NAME
                && (1..=MAX_ATTACHMENT).contains(&attachment.bytes)
                && !attachment.binary
        })
}

#[cfg(any(windows, target_os = "linux"))]
fn desktop_request(text: &str) -> agent::task_tools::DesktopRequest {
    let lower = text.to_lowercase();
    let capture = requested_desktop_capture(text);
    #[cfg(windows)]
    let reveal = capture
        && [
            "düz masaüst",
            "boş masaüst",
            "masaüstüne gel",
            "masaüstüne geç",
            "masaüstünü göster",
            "show the desktop",
            "show desktop",
            "bare desktop",
            "unobstructed desktop",
        ]
        .iter()
        .any(|phrase| lower.contains(phrase));
    let text_file = [
        "masaüstüne",
        "masaüstünde",
        "on the desktop",
        "to the desktop",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase))
        && [".txt", "metin dosyası", "metin belgesi", "text file"]
            .iter()
            .any(|phrase| lower.contains(phrase))
        && ["oluştur", "yazan", "yaz", "create", "write", "kaydet"]
            .iter()
            .any(|word| lower.contains(word))
        && !["oluşturma", "yazma", "kaydetme", "don't", "do not"]
            .iter()
            .any(|word| lower.contains(word));
    let image_before_text = text_file
        && capture
        && ["ardından", "en son", "sonra", "then", "after"]
            .iter()
            .any(|word| lower.contains(word));
    agent::task_tools::DesktopRequest {
        #[cfg(windows)]
        reveal,
        text_file,
        image_before_text,
    }
}

fn workspace_continue(text: &str) -> Option<&str> {
    task_command(text, &["/devam"])
}

fn workspace_steer(text: &str) -> Option<&str> {
    task_command(text, &["/yönlendir", "/yonlendir"])
}

fn workspace_steer_for_job<'a>(job: &WorkspaceJob, message: &'a Message) -> Option<&'a str> {
    workspace_command_matches(job, message)
        .then(|| workspace_steer(&message.text))
        .flatten()
}

fn workspace_command_matches(job: &WorkspaceJob, message: &Message) -> bool {
    message.room == job.selection.room
        && message.author.data.key == job.actor
        && message
            .reply
            .as_ref()
            .is_some_and(|quote| quote.id == job.source)
}

#[derive(Clone, Copy)]
enum WorkspaceControl {
    Status,
    Stop,
}

impl WorkspaceControl {
    fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "/durum" => Some(Self::Status),
            "/durdur" => Some(Self::Stop),
            _ => None,
        }
    }
}

fn task_command<'a>(text: &'a str, verbs: &[&str]) -> Option<&'a str> {
    let text = text.trim();
    let (verb, task) = text.split_once(char::is_whitespace)?;
    (verbs.contains(&verb) && !task.trim().is_empty()).then_some(task.trim())
}

#[cfg(all(test, windows))]
mod desktop_acceptance;

#[cfg(test)]
mod task_file_acceptance;

#[cfg(test)]
mod task_question_acceptance;

#[cfg(test)]
mod automation_acceptance;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshot_preapproval_matches_direct_requests_only() {
        for request in [
            "Send me a screenshot of the desktop.",
            "Send me a screen shot of the desktop.",
            "First take a desktop screenshot and send it, then create a text file on the desktop.",
            "Masaüstü görüntüsünü çekip bana at.",
            "Önce masaüstüne geç ve görüntüyü gönder, sonra masaüstüne bir metin dosyası oluştur.",
        ] {
            assert!(requested_desktop_capture(request), "{request}");
        }
        for request in [
            "What is a desktop screenshot?",
            "Masaüstü görüntüsü hata veriyor.",
            "Send a report on desktop screenshots.",
            "Do not send a desktop screenshot.",
            "Send a desktop screenshot. Actually, don't.",
            "Masaüstü görüntüsünü bana atma.",
            "Should I run \"send a desktop screenshot\"?",
            "> Send a desktop screenshot.",
            "'Send a desktop screenshot.'",
            "Masaüstü görüntüsünü nasıl gönder?",
        ] {
            assert!(!requested_desktop_capture(request), "{request}");
        }
    }

    #[test]
    fn steering_requires_the_paired_operator_room_and_original_quote() -> Result<()> {
        let operator = Identity::generate(Role::Human, "operator", "Operator")?;
        let other = Identity::generate(Role::Human, "other", "Other")?;
        let room = Uuid::new_v4();
        let job = WorkspaceJob {
            automation: None,
            id: Uuid::new_v4(),
            selection: Selection {
                host: "host".into(),
                room,
            },
            source: 21,
            actor: operator.id().into(),
            task: "Inspect this".into(),
            directory: PathBuf::new(),
            continuation: None,
            recovery: None,
            approval_round: 0,
            attachment: None,
            web_research: false,
            network: false,
            delegate: false,
            model: None,
            plan_only: false,
            review: None,
            skill_names: Vec::new(),
            forked: false,
            status: WorkspaceJobStatus::Running,
        };
        let mut message = Message {
            mode: None,
            withdrawn: false,
            undo_until_ms: 0,
            seq: 22,
            room,
            client_id: Uuid::new_v4(),
            author: operator.card.clone(),
            text: "/yönlendir sonuçları listele".into(),
            sent_at: now(),
            reply: Some(Quote {
                id: 21,
                author: operator.card.clone(),
                text: "incele".into(),
            }),
            root: None,
            invitation: None,
            attachment: None,
        };
        assert_eq!(
            workspace_steer_for_job(&job, &message),
            Some("sonuçları listele")
        );
        message.author = other.card.clone();
        assert!(workspace_steer_for_job(&job, &message).is_none());
        message.author = operator.card.clone();
        message.room = Uuid::new_v4();
        assert!(workspace_steer_for_job(&job, &message).is_none());
        message.room = room;
        message.reply = None;
        assert!(workspace_steer_for_job(&job, &message).is_none());
        Ok(())
    }

    fn wait(session: &Session, predicate: impl Fn(&View) -> bool) -> Result<View> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let view = session
                .receive
                .recv_timeout(remaining)
                .context("Client update timed out")?;
            if predicate(&view) {
                return Ok(view);
            }
        }
    }

    #[test]
    fn operator_can_check_and_cancel_a_quoted_task_before_approval() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let operator = Identity::generate(Role::Human, "deniz", "Deniz")?;
        let agent = Identity::generate(Role::Agent, "poyraz", "Poyraz")?;
        let host = transport::start(&temp.path().join("host"), "127.0.0.1:0".parse()?, &operator)?;
        let mut sender = Connection::new(host.address.clone())?;
        let (_, ticket) = sender.offer(&operator, agent.card.clone(), None)?;
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
        fs::create_dir(&workspace)?;
        let session = Session::new(temp.path().join("profiles"), agent.clone(), None)?;
        let requester = Session::new(temp.path().join("profiles"), operator.clone(), None)?;
        requester
            .send
            .send(Command::Import(pack("host", &host.address)?))?;
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
            .send(Command::Import(pack("host", &host.address)?))?;

        let mut send = |text: &str, reply| -> Result<Message> {
            sender.call(
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
        let source = send("Masaüstünün ekran görüntüsünü at.", None)?;
        wait(&session, |view| {
            view.local
                .jobs
                .iter()
                .any(|job| job.source == source.seq && job.status == WorkspaceJobStatus::Pending)
        })?;
        let has_activity = |view: &View, phase| {
            view.local.hosts.get(&host.address.pin).is_some_and(|link| {
                link.supports_task_activity
                    && link.task_activity.iter().any(|activity| {
                        activity.source == source.seq
                            && activity.room == room
                            && activity.phase == phase
                    })
            })
        };
        wait(&requester, |view| has_activity(view, TaskPhase::Pending))?;
        let activity = TaskActivity {
            room,
            source: source.seq,
            phase: TaskPhase::Running,
            plan: Some([1, 2]),
        };
        let mut requester_connection = Connection::new(host.address.clone())?;
        assert!(
            requester_connection
                .call::<()>(
                    &operator,
                    Action::PublishTaskActivity {
                        activity: activity.clone()
                    }
                )
                .is_err()
        );
        let outsider = Identity::generate(Role::Agent, "ayaz", "Ayaz")?;
        let mut outsider_connection = Connection::new(host.address.clone())?;
        assert!(
            outsider_connection
                .call::<()>(&outsider, Action::PublishTaskActivity { activity })
                .is_err()
        );
        let outside: Sync = outsider_connection.call(
            &outsider,
            Action::Sync {
                after: 0,
                grants: vec![],
                read: BTreeMap::new(),
                avatar: None,
            },
        )?;
        assert!(outside.task_activity.is_empty());
        let unquoted = send("/durdur", None)?;
        let status = send("/durum", Some(source.seq))?;
        let has_receipt = |view: &View, seq, text: &str| {
            view.local.hosts[&host.address.pin]
                .messages
                .values()
                .any(|message| {
                    message.author.data.key == agent.id()
                        && message.reply.as_ref().is_some_and(|quote| quote.id == seq)
                        && message.text == text
                })
        };
        let view = wait(&session, |view| {
            has_receipt(view, status.seq, "Görev yerel onay bekliyor.")
        })?;
        assert_eq!(view.local.jobs[0].status, WorkspaceJobStatus::Pending);
        let stop = send("/durdur", Some(source.seq))?;
        let view = wait(&session, |view| {
            has_receipt(view, stop.seq, "Görev başlamadan iptal edildi.")
        })?;
        assert_eq!(view.local.jobs.len(), 1);
        assert_eq!(view.local.jobs[0].status, WorkspaceJobStatus::Cancelled);
        assert!(view.local.hosts[&host.address.pin].task_activity.is_empty());
        let messages = &view.local.hosts[&host.address.pin].messages;
        assert!(!messages.values().any(|message| {
            message
                .reply
                .as_ref()
                .is_some_and(|quote| quote.id == unquoted.seq)
        }));
        wait(&requester, |view| has_activity(view, TaskPhase::Cancelled))?;
        let id = view.local.jobs[0].id;
        session.send.send(Command::RetryWorkspaceJob(id))?;
        let retried = wait(&session, |view| {
            view.local.jobs[0].status == WorkspaceJobStatus::Pending
        })?;
        assert_eq!(retried.local.jobs.len(), 1);
        assert_eq!(retried.local.jobs[0].id, id);
        wait(&requester, |view| has_activity(view, TaskPhase::Pending))?;
        Ok(())
    }

    #[test]
    fn desktop_client_keeps_archive_and_reconnects_its_outbox() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let alice = Identity::generate(Role::Human, "alice", "Alice")?;
        let bob = Identity::generate(Role::Human, "bobby", "Bob")?;
        let inbox = directory.path().join("inbox");
        let profiles = directory.path().join("profiles");
        let sender = Session::new(profiles.clone(), alice.clone(), Some(inbox.clone()))?;
        assert!(Session::new(profiles.clone(), alice.clone(), None).is_err());
        let guest = Session::new(profiles.clone(), bob.clone(), Some(inbox))?;
        sender.send.send(Command::Start("127.0.0.1:0".parse()?))?;
        let address = wait(&sender, |view| view.host.is_some())?.host.unwrap();
        sender
            .send
            .send(Command::Import(pack("card", &bob.card)?))?;
        wait(&sender, |view| view.local.contacts.contains_key(bob.id()))?;
        sender.send.send(Command::Offer(bob.card.clone()))?;
        wait(&sender, |view| view.copy.is_some())?;
        let view = wait(&guest, |view| {
            view.local
                .hosts
                .get(&address.pin)
                .is_some_and(|link| !link.requests.is_empty())
        })?;
        let request = view.local.hosts[&address.pin].requests[0].clone();
        guest.send.send(Command::Decide(
            address.pin.clone(),
            Box::new(request.clone()),
            true,
        ))?;
        let room = request.terms.data.id;
        let selection = Selection {
            host: address.pin.clone(),
            room,
        };
        wait(&sender, |view| {
            view.local.hosts[&address.pin]
                .rooms
                .iter()
                .any(|entry| entry.id == room)
        })?;
        sender.send.send(Command::SendDraft(
            selection.clone(),
            Uuid::new_v4(),
            "İlk mesaj".into(),
            None,
            None,
        ))?;
        let before_clear = wait(&guest, |view| {
            view.local.hosts[&address.pin].last(room).is_some()
        })?;
        let first_seq = before_clear.local.hosts[&address.pin]
            .last(room)
            .unwrap()
            .seq;
        guest.send.send(Command::Archive(selection.clone(), true))?;
        wait(&guest, |view| {
            view.local.hosts[&address.pin].archived.contains(&room)
        })?;
        sender.send.send(Command::Stop)?;
        wait(&sender, |view| view.host.is_none())?;
        guest.send.send(Command::ClearChat(selection.clone()))?;
        let cleared = wait(&guest, |view| {
            view.local.hosts[&address.pin].cleared_through(room) == first_seq
        })?;
        let link = &cleared.local.hosts[&address.pin];
        assert!(link.last(room).is_none());
        assert_eq!(link.unread(room, bob.id()), 0);
        guest.send.send(Command::SendDraft(
            selection.clone(),
            Uuid::new_v4(),
            "Çevrimdışı yanıt".into(),
            None,
            None,
        ))?;
        wait(&guest, |view| {
            !view.local.hosts[&address.pin].queue.is_empty()
        })?;
        sender.send.send(Command::Start(address.socket))?;
        wait(&sender, |view| {
            view.local.hosts[&address.pin].messages.len() == 2
        })?;
        let view = wait(&guest, |view| {
            let link = &view.local.hosts[&address.pin];
            link.queue.is_empty() && link.messages.len() == 2
        })?;
        assert!(view.local.hosts[&address.pin].archived.contains(&room));
        assert_eq!(view.local.hosts[&address.pin].unread(room, bob.id()), 0);
        assert_eq!(
            view.local.hosts[&address.pin]
                .visible_messages(room)
                .count(),
            1
        );
        guest
            .send
            .send(Command::History(selection.clone(), first_seq + 1))?;
        wait(&guest, |view| {
            view.notice.as_deref() == Some("Daha eski görünür mesaj yok.")
        })?;
        assert!(!view.local.contacts.contains_key(bob.id()));
        assert!(view.local.friend(bob.id(), alice.id()).is_some());
        let restored = Session::new(directory.path().join("fresh"), bob.clone(), None)?;
        restored
            .send
            .send(Command::Import(pack("host", &address)?))?;
        let restored = wait(&restored, |view| {
            view.local.friend(bob.id(), alice.id()).is_some()
        })?;
        assert_eq!(restored.local.hosts[&address.pin].messages.len(), 2);
        sender.send.send(Command::Stop)?;
        wait(&sender, |view| view.host.is_none())?;
        for (identity, other, name, archive) in [
            (&alice, &bob, "alice", false),
            (&bob, &alice, "bobby", true),
        ] {
            let db = Database::open(profiles.join(identity.id()).join("client.sqlite3"))?;
            let data: String =
                db.query_row("SELECT data FROM client WHERE id=1", [], |row| row.get(0))?;
            let local: Local = serde_json::from_str(&data)?;
            assert!(local.friend(identity.id(), other.id()).is_some(), "{name}");
            assert!(local.contacts.contains_key(other.id()), "{name}");
            assert!(!local.contacts.contains_key(identity.id()), "{name}");
            let link = &local.hosts[&address.pin];
            assert_eq!(link.archived.contains(&room), archive, "{name}");
            assert_eq!(link.requests[0].status, Status::Accepted, "{name}");
            assert_eq!(link.messages.len(), 2, "{name}");
            assert_eq!(
                link.visible_messages(room).count(),
                if archive { 1 } else { 2 },
                "{name}"
            );
        }
        Ok(())
    }
}
