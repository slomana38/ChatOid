use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::Read,
    net::SocketAddr,
    path::PathBuf,
    sync::mpsc,
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
    identity::{Card, Identity, Role, pack, unpack, write_new},
    protocol::*,
    transport::{self, Connection, RemoteError, RunningHost},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    pub host: String,
    pub room: Uuid,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Pending {
    pub draft: Draft,
    pub error: Option<String>,
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
    completed: BTreeMap<String, Option<String>>,
    pub opened_invites: BTreeSet<Uuid>,
}

impl Local {
    pub fn friend(&self, me: &str, other: &str) -> Option<Friendship> {
        self.hosts
            .values()
            .flat_map(|host| &host.requests)
            .filter_map(Request::friendship)
            .find(|proof| proof.check(me, other).is_ok())
    }
    pub fn already_requested(&self, me: &str, other: &str) -> bool {
        self.hosts
            .values()
            .flat_map(|host| &host.requests)
            .any(|r| {
                let t = &r.terms.data;
                t.room.is_none()
                    && matches!(r.status, Status::Pending | Status::Accepted)
                    && ((t.from == me && t.to == other) || (t.from == other && t.to == me))
            })
    }
}

pub enum Command {
    Start(SocketAddr),
    Stop,
    Import(String),
    Pair(String),
    Offer(Card),
    Group(String),
    Invite(Selection, Card),
    Decide(String, Box<Request>, bool),
    Send(Selection, String, Option<i64>),
    History(Selection, i64),
    Archive(Selection, bool),
    Read(Selection, i64),
    Leave(Selection),
    Remove(Selection, String),
    Close(Selection),
    Retry(Selection, Uuid),
    Settings(Settings),
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
                }
                .run(input, output)
            })?;
        Ok(Self { send, receive })
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
            if self.receive_model() {
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
        let after = self.local.hosts[pin].cursor;
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
            },
        )?;
        let link = self.local.hosts.get_mut(pin).expect("link present");
        link.cursor = sync.cursor;
        link.rooms = sync.rooms;
        link.requests = sync.requests;
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
        Ok(())
    }

    fn poll(&mut self) {
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
            .push(Pending { draft, error: None });
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

    fn offer(&mut self, card: Card, room: Option<Selection>) -> Result<()> {
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
        let (request, ticket) = self
            .connect(&host.pin)?
            .offer(&actor, card.clone(), group)?;
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
                    },
                )?;
            }
        }
        self.refresh(&host.pin)
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
            let conflicting = self
                .local
                .hosts
                .values()
                .flat_map(|h| &h.requests)
                .any(|r| {
                    let other = &r.terms.data;
                    other.id != terms.id
                        && other.room.is_none()
                        && ((other.from == terms.from && other.to == terms.to)
                            || (other.from == terms.to && other.to == terms.from))
                        && (r.status == Status::Accepted
                            || (r.status == Status::Pending
                                && (other.from.as_str(), other.id)
                                    < (terms.from.as_str(), terms.id)))
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
            Command::Offer(card) => self.offer(card, None)?,
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
            Command::Invite(selection, card) => self.offer(card, Some(selection))?,
            Command::Decide(host, request, accepted) => self.decide(&host, *request, accepted)?,
            Command::Send(s, text, reply) => {
                ensure!(
                    !text.trim().is_empty() && text.chars().count() <= MAX_TEXT,
                    "Mesaj 1–4.000 karakter olmalı"
                );
                self.queue(
                    &s.host,
                    Draft {
                        id: Uuid::new_v4(),
                        room: s.room,
                        text,
                        reply,
                        invitation: None,
                    },
                )?;
                if self.local.hosts[&s.host].online {
                    self.flush(&s.host)?;
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
                        self.decide(&host, *request, accepted)
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
        sender
            .send
            .send(Command::Send(selection.clone(), "İlk mesaj".into(), None))?;
        wait(&guest, |view| {
            view.local.hosts[&address.pin].last(room).is_some()
        })?;
        guest.send.send(Command::Archive(selection.clone(), true))?;
        wait(&guest, |view| {
            view.local.hosts[&address.pin].archived.contains(&room)
        })?;
        sender.send.send(Command::Stop)?;
        wait(&sender, |view| view.host.is_none())?;
        guest.send.send(Command::Send(
            selection.clone(),
            "Çevrimdışı yanıt".into(),
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
