use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::{Read, Write},
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
    protocol::*,
    transport::{self, Connection, RemoteError, RunningHost},
};

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
    pub receipts: Vec<Receipt>,
    #[serde(default)]
    pub attachments: bool,
    #[serde(default)]
    pub max_file_bytes: usize,
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
            receipts: vec![],
            attachments: false,
            max_file_bytes: 0,
            avatars: BTreeMap::new(),
            avatar_current: None,
            online: false,
            error: String::new(),
        }
    }
    pub fn unread(&self, room: Uuid, viewer: &str) -> usize {
        let after = *self.seen.get(&room).unwrap_or(&0);
        self.messages
            .range((after + 1)..)
            .filter(|(_, m)| m.room == room && m.author.data.key != viewer)
            .count()
    }
    pub fn last(&self, room: Uuid) -> Option<&Message> {
        self.messages.values().rev().find(|m| m.room == room)
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Local {
    pub hosts: BTreeMap<String, Link>,
    pub contacts: BTreeMap<String, Card>,
    pub grants: Vec<Grant>,
    pub operator: Option<Card>,
    pub tickets: BTreeMap<Uuid, Ticket>,
    pub bind: Option<SocketAddr>,
    pub settings: Settings,
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
    Download(Selection, i64, PathBuf),
    SetAvatar(Option<PathBuf>),
    SetBuiltInAvatar(usize),
    SetDisplayName(String),
    SetDeviceSharing(bool),
    LookupProfile(String),
    SetDiscovery(bool),
    History(Selection, i64),
    Archive(Selection, bool),
    Read(Selection, i64),
    Leave(Selection),
    Remove(Selection, String),
    Close(Selection),
    Retry(Selection, Uuid),
    CancelSend(Selection, Uuid),
    Settings(Settings),
    SignInCodex(String),
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
    SignIn {
        codex: String,
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

pub struct Session {
    pub send: mpsc::Sender<Command>,
    pub receive: mpsc::Receiver<View>,
    transfers: Arc<Mutex<Transfers>>,
}

impl Session {
    pub fn new(path: PathBuf, identity: Identity, inbox: Option<PathBuf>) -> Result<Self> {
        let dir = path.join(identity.id());
        fs::create_dir_all(&dir)?;
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
}

struct UploadJob {
    pin: String,
    id: Uuid,
    result: mpsc::Receiver<Result<Message>>,
}

impl Worker {
    fn run(mut self, input: mpsc::Receiver<Command>, output: mpsc::Sender<View>) {
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
                next = Instant::now() + Duration::from_secs(2);
                edited = true;
            }
            if self.receive_upload() | self.receive_model() {
                edited = true;
            }
            if self.schedule() {
                edited = true;
            }
            if edited && !self.publish(&output) {
                break;
            }
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
            if path.extension().and_then(|ext| ext.to_str()) != Some("invite")
                || !entry.file_type()?.is_file()
            {
                continue;
            }
            let mut contents = String::new();
            if fs::File::open(&path)
                .and_then(|file| file.take(32_769).read_to_string(&mut contents))
                .is_err()
            {
                continue;
            }
            let Ok(ticket) = unpack::<Ticket>("invite", &contents) else {
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

    fn queue(&mut self, pin: &str, draft: Draft) -> Result<()> {
        self.local
            .hosts
            .get_mut(pin)
            .context("Host bulunamadı")?
            .queue
            .push(Pending {
                draft,
                error: None,
                binary: None,
            });
        self.save()
    }

    fn flush(&mut self, pin: &str) -> Result<()> {
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
                                    address, &identity, &state, draft, &path,
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
            Command::SendDraft(s, client_id, text, reply, path) => {
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
                        metadata.is_file()
                            && (1..=MAX_BINARY_ATTACHMENT as u64).contains(&metadata.len()),
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
                                id: client_id,
                                room: s.room,
                                text,
                                reply,
                                invitation: None,
                                attachment: None,
                            },
                            error: None,
                            binary: Some(binary),
                        });
                    self.save()?;
                } else {
                    self.queue(
                        &s.host,
                        Draft {
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
            }
            Command::Download(s, seq, path) => {
                let info = self
                    .local
                    .hosts
                    .get(&s.host)
                    .and_then(|link| link.messages.get(&seq))
                    .filter(|m| m.room == s.room)
                    .and_then(|m| m.attachment.clone())
                    .context("Dosya bilgisi bulunamadı")?;
                let parent = path.parent().context("Hedef klasör bulunamadı")?;
                let mut output = tempfile::NamedTempFile::new_in(parent)?;
                if info.binary {
                    ensure!(
                        (1..=MAX_BINARY_ATTACHMENT).contains(&info.bytes),
                        "Geçersiz dosya boyutu"
                    );
                    let identity = self.identity.clone();
                    let mut offset = 0;
                    while offset < info.bytes {
                        let chunk = self
                            .connect(&s.host)?
                            .download_chunk(&identity, seq, offset)?;
                        ensure!(
                            chunk.len() == (info.bytes - offset).min(FILE_CHUNK),
                            "Dosya eksik"
                        );
                        output.write_all(&chunk)?;
                        offset += chunk.len();
                    }
                } else {
                    let file: Attachment =
                        self.call(&s.host, Action::Download { room: s.room, seq })?;
                    output.write_all(&file.bytes()?)?;
                }
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
                let messages: Vec<Message> = self.call(
                    &s.host,
                    Action::History {
                        room: s.room,
                        before,
                    },
                )?;
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
                if let Some(link) = self.local.hosts.get_mut(&s.host) {
                    link.queue
                        .retain(|item| item.draft.id != id || item.draft.room != s.room);
                }
                let mut transfers = self.transfers.lock().expect("file progress lock");
                if !transfers.active.contains(&id) {
                    transfers.cancelled.remove(&id);
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
                if !settings.workspace.is_empty() {
                    ensure!(
                        std::path::Path::new(&settings.workspace).is_dir(),
                        "Çalışma klasörü bulunamadı"
                    );
                }
                self.local.settings = settings;
                self.notice = Some("Ajan ayarları kaydedildi.".into());
            }
            Command::SignInCodex(codex) => {
                ensure!(
                    self.identity.card.data.role == Role::Agent,
                    "Ajan hesabı gerekli"
                );
                ensure!(self.model.is_none(), "Codex meşgul");
                self.spawn_model(move || {
                    let status = agent::sign_in(&codex);
                    (ModelTask::SignIn { codex }, status)
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
        if self.identity.card.data.role != Role::Agent
            || !self.local.settings.enabled
            || self.model.is_some()
        {
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

    fn receive_model(&mut self) -> bool {
        let Some(receiver) = self.model.as_ref() else {
            return false;
        };
        let (task, status) = match receiver.try_recv() {
            Ok(value) => value,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.model = None;
                self.codex = "Codex bağlantısı hatalı".into();
                self.notice = Some("Codex işlemi kesildi.".into());
                return true;
            }
        };
        self.model = None;
        self.codex = "Hazır".into();
        match task {
            ModelTask::SignIn { codex } => match status {
                Ok(()) => {
                    self.local.settings.codex = codex;
                    self.codex = "OpenAI hesabı bağlı".into();
                    self.notice = Some("OpenAI girişi tamamlandı.".into());
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

fn authority_pending(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<RemoteError>()
        .is_some_and(|error| error.status == reqwest::StatusCode::SERVICE_UNAVAILABLE)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        wait(&guest, |view| {
            view.local.hosts[&address.pin].last(room).is_some()
        })?;
        guest.send.send(Command::Archive(selection.clone(), true))?;
        wait(&guest, |view| {
            view.local.hosts[&address.pin].archived.contains(&room)
        })?;
        sender.send.send(Command::Stop)?;
        wait(&sender, |view| view.host.is_none())?;
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
        assert_eq!(view.local.hosts[&address.pin].unread(room, bob.id()), 1);
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
        }
        Ok(())
    }
}
