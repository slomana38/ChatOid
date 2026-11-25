use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, ensure};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path as RoutePath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    identity::{Card, Role, decode, encode, random, verify},
    protocol::*,
};

const SCHEMA: &str = "
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
CREATE TABLE IF NOT EXISTS host (owner TEXT NOT NULL, pin TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS profiles (id TEXT PRIMARY KEY, card TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS rooms (
  id TEXT PRIMARY KEY, name TEXT NOT NULL, owner TEXT NOT NULL, is_group INTEGER NOT NULL,
  closed INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS members (
  room TEXT NOT NULL REFERENCES rooms(id), actor TEXT NOT NULL REFERENCES profiles(id),
  after_seq INTEGER NOT NULL, through_seq INTEGER);
CREATE INDEX IF NOT EXISTS member_lookup ON members(actor, room);
CREATE UNIQUE INDEX IF NOT EXISTS active_member ON members(room, actor) WHERE through_seq IS NULL;
CREATE TABLE IF NOT EXISTS requests (
  id TEXT PRIMARY KEY, sender TEXT NOT NULL, recipient TEXT NOT NULL, room TEXT,
  code_hash TEXT NOT NULL, redeemed INTEGER NOT NULL DEFAULT 0, expires INTEGER NOT NULL,
  result INTEGER, data TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS request_lookup ON requests(recipient, sender);
CREATE TABLE IF NOT EXISTS messages (
  seq INTEGER PRIMARY KEY AUTOINCREMENT, room TEXT NOT NULL REFERENCES rooms(id),
  actor TEXT NOT NULL REFERENCES profiles(id), client_id TEXT NOT NULL, time INTEGER NOT NULL,
  payload TEXT NOT NULL, reply INTEGER REFERENCES messages(seq), root INTEGER REFERENCES messages(seq),
  sent_ms INTEGER NOT NULL DEFAULT 0, undo_deadline_ms INTEGER NOT NULL DEFAULT 0,
  withdrawn INTEGER NOT NULL DEFAULT 0,
  UNIQUE(actor, client_id));
CREATE INDEX IF NOT EXISTS room_messages ON messages(room, seq);
CREATE UNIQUE INDEX IF NOT EXISTS source_agent_reply ON messages(actor, reply) WHERE root IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS root_agent_reply ON messages(actor, root) WHERE root IS NOT NULL;
CREATE TABLE IF NOT EXISTS attachments (
  seq INTEGER PRIMARY KEY REFERENCES messages(seq), name TEXT NOT NULL,
  data TEXT NOT NULL, bytes INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS friendship_checks (
  request TEXT PRIMARY KEY REFERENCES requests(id), proof TEXT NOT NULL, authority TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS introductions (
  id TEXT PRIMARY KEY, sender TEXT NOT NULL, expires INTEGER NOT NULL, ticket TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS public_profiles (
  actor TEXT PRIMARY KEY REFERENCES profiles(id), publication TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS binary_files (
  seq INTEGER PRIMARY KEY REFERENCES attachments(seq), hash TEXT NOT NULL, data BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS upload_chunks (
  actor TEXT NOT NULL REFERENCES profiles(id), id TEXT NOT NULL, offset INTEGER NOT NULL,
  data BLOB NOT NULL, updated INTEGER NOT NULL, PRIMARY KEY(actor,id,offset));
CREATE TABLE IF NOT EXISTS attachment_previews (
  seq INTEGER PRIMARY KEY REFERENCES attachments(seq), preview TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS avatars (
  actor TEXT PRIMARY KEY REFERENCES profiles(id), digest TEXT NOT NULL, data TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS delivery (actor TEXT PRIMARY KEY REFERENCES profiles(id), seq INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS reads (
  actor TEXT NOT NULL REFERENCES profiles(id), room TEXT NOT NULL REFERENCES rooms(id),
  seq INTEGER NOT NULL, PRIMARY KEY(actor, room));
CREATE TABLE IF NOT EXISTS task_activity (
  source INTEGER PRIMARY KEY REFERENCES messages(seq), room TEXT NOT NULL REFERENCES rooms(id),
  agent TEXT NOT NULL REFERENCES profiles(id), recipient TEXT NOT NULL REFERENCES profiles(id),
  updated INTEGER NOT NULL, data TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS activity_recipient ON task_activity(recipient, updated);
CREATE INDEX IF NOT EXISTS activity_expiry ON task_activity(updated);
";

const VISIBLE: &str = "
EXISTS (SELECT 1 FROM members m WHERE m.room = messages.room AND m.actor = ?1
  AND messages.seq > m.after_seq AND (m.through_seq IS NULL OR messages.seq <= m.through_seq))";

const SHARED_AVATAR: &str = "(
  avatars.actor=?1 OR EXISTS(SELECT 1 FROM members peer JOIN members own ON own.room=peer.room
    WHERE peer.actor=avatars.actor AND own.actor=?1 AND peer.through_seq IS NULL AND own.through_seq IS NULL
  )
)";

#[derive(Default)]
struct Auth {
    challenges: HashMap<String, i64>,
    sessions: HashMap<String, (Card, i64)>,
}

pub struct Host {
    db: Mutex<Connection>,
    auth: Mutex<Auth>,
    owner: String,
    pin: String,
}

pub fn digest(bytes: impl AsRef<[u8]>) -> String {
    encode(Sha256::digest(bytes))
}

impl Host {
    pub fn open(path: &Path, owner: &Card, pin: &str) -> Result<Self> {
        owner.check()?;
        let db = Connection::open(path)?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch(SCHEMA)?;
        ensure_message_columns(&db)?;
        let binding: Option<(String, String)> = db
            .query_row("SELECT owner,pin FROM host", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        if let Some((bound_owner, bound_pin)) = binding {
            ensure!(
                bound_owner == owner.data.key && bound_pin == pin,
                "Host kimliği veritabanıyla uyuşmuyor"
            );
        } else {
            db.execute(
                "INSERT INTO host VALUES(?1,?2)",
                params![owner.data.key, pin],
            )?;
        }
        remember(&db, owner)?;
        Ok(Self {
            db: Mutex::new(db),
            auth: Mutex::new(Auth::default()),
            owner: owner.data.key.clone(),
            pin: pin.into(),
        })
    }

    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/v1/challenge", post(challenge))
            .route("/v1/login", post(login))
            .route("/v1/action", post(action))
            .route("/v1/friendship", post(friendship))
            .route("/v1/upload/{id}/{offset}", post(upload))
            .route("/v1/file/{seq}/{offset}", get(download_binary))
            .layer(DefaultBodyLimit::max(2 * MAX_ATTACHMENT))
            .with_state(self)
    }

    fn challenge(&self) -> Result<String> {
        let mut auth = self.auth.lock().expect("auth lock");
        auth.challenges.retain(|_, deadline| *deadline > now());
        ensure!(auth.challenges.len() < 1024, "Çok fazla giriş denemesi");
        let nonce = encode(random::<32>()?);
        auth.challenges.insert(nonce.clone(), now() + 30);
        Ok(nonce)
    }

    fn login(&self, login: SignIn) -> Result<String> {
        let SignIn { card, proof } = login;
        card.check()?;
        ensure!(
            proof.data.host == self.pin && proof.data.actor == card.data.key,
            "Oturum host ile uyuşmuyor"
        );
        verify(&card.data.key, "login", &proof)?;
        let mut auth = self.auth.lock().expect("auth lock");
        ensure!(
            auth.challenges
                .remove(&proof.data.nonce)
                .is_some_and(|deadline| deadline > now()),
            "Giriş kodu geçersiz"
        );
        auth.sessions.retain(|_, (_, deadline)| *deadline > now());
        ensure!(auth.sessions.len() < 1024, "Çok fazla açık oturum");
        remember(&self.db.lock().expect("database lock"), &card)?;
        let token = encode(random::<32>()?);
        auth.sessions
            .insert(digest(&token), (card, now() + 12 * 3600));
        Ok(token)
    }

    fn auth(&self, headers: &HeaderMap) -> Option<Card> {
        let token = headers
            .get("authorization")?
            .to_str()
            .ok()?
            .strip_prefix("Bearer ")?;
        let auth = self.auth.lock().ok()?;
        let (card, expiry) = auth.sessions.get(&digest(token))?;
        (*expiry > now()).then(|| card.clone())
    }

    fn apply(&self, actor: &Card, action: Action) -> Result<Value> {
        self.check_group_relationship(actor, &action)?;
        let mut connection = self.db.lock().expect("database lock");
        let db = connection.transaction()?;
        let id = &actor.data.key;
        let value = match action {
            Action::PublishTaskActivity { activity } => {
                ensure!(actor.data.role == Role::Agent, "Ajan hesabı gerekli");
                publish_task_activity(&db, id, activity)?;
                Value::Null
            }
            Action::Sync {
                after,
                grants,
                read,
                avatar,
            } => self.sync(&db, id, after, grants, read, avatar)?,
            Action::History { room, before } => {
                ensure!(ever_member(&db, room, id)?, "Sohbet erişimi yok");
                let ids = message_ids(
                    &db,
                    &format!(
                        "SELECT seq FROM messages WHERE {VISIBLE} AND room=?2 AND seq < ?3 ORDER BY seq DESC LIMIT 200"
                    ),
                    params![id, room.to_string(), before],
                )?;
                serde_json::to_value(
                    ids.into_iter()
                        .rev()
                        .map(|seq| message(&db, id, seq))
                        .collect::<Result<Vec<_>>>()?,
                )?
            }
            Action::Group { id: room, name } => {
                ensure!(
                    id == &self.owner && actor.data.role == Role::Human,
                    "Grubu host sahibi kurabilir"
                );
                let name = clean(&name, 80)?;
                let created = db.execute(
                    "INSERT OR IGNORE INTO rooms(id,name,owner,is_group) VALUES(?1,?2,?3,1)",
                    params![room.to_string(), name, id],
                )?;
                if created != 0 {
                    add_member(&db, room, id)?;
                }
                json!(room)
            }
            Action::Offer {
                terms,
                target,
                friendship,
                code_hash,
                authority,
            } => {
                let id = terms.data.id;
                let result =
                    self.offer(&db, actor, terms, target, friendship.as_deref(), &code_hash)?;
                if let (Some(proof), Some(authority)) = (friendship, authority) {
                    db.execute(
                        "INSERT OR IGNORE INTO friendship_checks VALUES(?1,?2,?3)",
                        params![
                            id.to_string(),
                            serde_json::to_string(&proof)?,
                            serde_json::to_string(&authority)?
                        ],
                    )?;
                }
                result
            }
            Action::Redeem {
                id: request,
                code,
                grant,
            } => self.redeem(&db, id, request, &code, grant.as_ref())?,
            Action::Decide { decision } => self.decide(&db, id, decision)?,
            Action::Send { draft } => serde_json::to_value(send(&db, actor, draft, None)?)?,
            Action::WithdrawMessage { room, seq } => {
                ensure!(seq > 0, "Mesaj bulunamadı");
                let (author, room_id, deadline, withdrawn): (String, String, i64, bool) = db
                    .query_row(
                        "SELECT actor,room,undo_deadline_ms,withdrawn FROM messages WHERE seq=?1",
                        [seq],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .optional()?
                    .context("Mesaj bulunamadı")?;
                ensure!(
                    author == *id && room_id == room.to_string(),
                    "Yalnızca kendi mesajını geri alabilirsin"
                );
                ensure!(visible(&db, id, seq)?, "Mesaja erişim yok");
                if !withdrawn {
                    ensure!(now_ms() <= deadline, "Mesajı geri alma süresi doldu");
                    let raw: String =
                        db.query_row("SELECT payload FROM messages WHERE seq=?1", [seq], |r| {
                            r.get(0)
                        })?;
                    let mut draft: Draft = serde_json::from_str(&raw)?;
                    draft.text.clear();
                    draft.mode = None;
                    draft.attachment = None;
                    draft.invitation = None;
                    db.execute(
                        "UPDATE messages SET withdrawn=1,payload=?2 WHERE seq=?1",
                        params![seq, serde_json::to_string(&draft)?],
                    )?;
                    db.execute("DELETE FROM binary_files WHERE seq=?1", [seq])?;
                    db.execute("DELETE FROM attachment_previews WHERE seq=?1", [seq])?;
                    db.execute("DELETE FROM attachments WHERE seq=?1", [seq])?;
                }
                Value::Null
            }
            Action::CanProcessMessage { room, seq } => {
                ensure!(actor.data.role == Role::Agent, "Ajan hesabı gerekli");
                let eligible: bool = db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM messages s JOIN rooms r ON r.id=s.room
                     WHERE s.seq=?1 AND s.room=?2 AND s.withdrawn=0 AND s.undo_deadline_ms<?3
                       AND r.closed=0 AND s.actor<>?4
                       AND EXISTS(SELECT 1 FROM members a WHERE a.room=s.room AND a.actor=?4 AND a.through_seq IS NULL AND s.seq>a.after_seq)
                       AND EXISTS(SELECT 1 FROM members h WHERE h.room=s.room AND h.actor=s.actor AND h.through_seq IS NULL AND s.seq>h.after_seq))",
                    params![seq, room.to_string(), now_ms(), id], |r| r.get(0),
                )?;
                json!(eligible)
            }
            Action::SendBinary {
                draft,
                name,
                bytes,
                hash,
            } => {
                check_filename(&name)?;
                ensure!(
                    draft.attachment.is_none()
                        && draft.invitation.is_none()
                        && (1..=MAX_BINARY_ATTACHMENT).contains(&bytes),
                    "Geçersiz dosya eki"
                );
                decode::<32>(&hash)?;
                let id = draft.id;
                let message = send(&db, actor, draft, Some((name, bytes, hash)))?;
                db.execute(
                    "DELETE FROM upload_chunks WHERE actor=?1 AND id=?2",
                    params![actor.data.key, id.to_string()],
                )?;
                serde_json::to_value(message)?
            }
            Action::Download { room, seq } => {
                ensure!(visible(&db, id, seq)?, "Dosyaya erişim yok");
                ensure!(!is_withdrawn(&db, seq)?, "Geri alınan mesaja erişilemiyor");
                let file = db.query_row(
                    "SELECT a.name,a.data FROM attachments a JOIN messages m ON m.seq=a.seq WHERE m.room=?1 AND m.seq=?2",
                    params![room.to_string(), seq],
                    |row| Ok(Attachment { name: row.get(0)?, data: row.get(1)? }),
                ).context("Mesajın dosya eki yok")?;
                json!(file)
            }
            Action::Avatar { actor, digest } => {
                let data: String = db.query_row(
                    &format!("SELECT data FROM avatars WHERE {SHARED_AVATAR} AND actor=?2 AND digest=?3"),
                    params![id, actor, digest], |row| row.get(0),
                ).context("Profil fotoğrafı bulunamadı")?;
                json!(AvatarPhoto { digest, data })
            }
            Action::Leave { room } => {
                let (owner, group, _) = room_settings(&db, room)?;
                ensure!(group && owner != *id, "Yalnızca grup üyesi ayrılabilir");
                finish_membership(&db, room, id)?;
                Value::Null
            }
            Action::Remove { room, member } => {
                let (owner, group, _) = room_settings(&db, room)?;
                ensure!(
                    group && owner == *id && member != *id,
                    "Yalnızca grup yöneticisi üye çıkarabilir"
                );
                finish_membership(&db, room, &member)?;
                Value::Null
            }
            Action::Close { room } => {
                let (owner, group, _) = room_settings(&db, room)?;
                ensure!(
                    group && owner == *id,
                    "Yalnızca grup yöneticisi grubu kapatabilir"
                );
                db.execute("UPDATE rooms SET closed=1 WHERE id=?1", [room.to_string()])?;
                Value::Null
            }
            Action::EndFriendship { room } => {
                let (_, group, _) = room_settings(&db, room)?;
                ensure!(
                    !group && ever_member(&db, room, id)?,
                    "Arkadaşlığı yalnızca katılımcılar sonlandırabilir"
                );
                db.execute("UPDATE rooms SET closed=1 WHERE id=?1", [room.to_string()])?;
                Value::Null
            }
            Action::DeliverIntroduction { route, ticket } => {
                ContactRoute::validate(&route)?;
                ensure!(
                    route.data.card.data.key == self.owner
                        && route.data.host.pin == self.pin
                        && actor.data.key != self.owner
                        && ticket.grant.is_none(),
                    "Bu kişi bağlantısı bu hosta ait değil"
                );
                ticket.host.check()?;
                db.execute("DELETE FROM introductions WHERE expires<=?1", [now()])?;
                let count: i64 = db.query_row(
                    "SELECT COUNT(*) FROM introductions WHERE sender=?1",
                    [id],
                    |row| row.get(0),
                )?;
                let existing: bool = db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM introductions WHERE id=?1 AND sender=?2)",
                    params![ticket.id.to_string(), id],
                    |row| row.get(0),
                )?;
                ensure!(
                    existing || count < 5,
                    "Bu adrese çok fazla istek gönderildi"
                );
                db.execute(
                    "INSERT OR IGNORE INTO introductions VALUES(?1,?2,?3,?4)",
                    params![
                        ticket.id.to_string(),
                        id,
                        route.data.expires.min(now() + INVITE_LIFETIME),
                        serde_json::to_string(&ticket)?,
                    ],
                )?;
                Value::Null
            }
            Action::PublishProfile { publication } => {
                verify(id, "shared-profile", &publication.profile)?;
                crate::identity::validate_name(&publication.profile.data.display_name)?;
                if let Some(profile) = &publication.profile.data.agent {
                    ensure!(actor.data.role == Role::Agent, "Ajan hesabı gerekli");
                    profile.validate()?;
                }
                if let Some(device) = &publication.device {
                    ensure!(actor.data.role == Role::Agent, "Ajan hesabı gerekli");
                    verify(id, "device-summary", device)?;
                    for value in [
                        &device.data.system,
                        &device.data.architecture,
                        &device.data.processor,
                        &device.data.memory,
                    ] {
                        ensure!(
                            value.len() <= 120 && !value.chars().any(char::is_control),
                            "Geçersiz cihaz özeti"
                        );
                    }
                }
                db.execute("INSERT INTO public_profiles VALUES(?1,?2) ON CONFLICT(actor) DO UPDATE SET publication=excluded.publication",
                    params![id,serde_json::to_string(&publication)?])?;
                Value::Null
            }
            Action::LoadProfile { actor } => {
                let shared: bool = db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM members a JOIN members b USING(room)
                     WHERE a.actor=?1 AND b.actor=?2 AND a.through_seq IS NULL AND b.through_seq IS NULL)",
                    params![id,actor], |row| row.get(0))?;
                ensure!(id == &actor || shared, "Profil erişimi yok");
                let raw = db
                    .query_row(
                        "SELECT publication FROM public_profiles WHERE actor=?1",
                        [&actor],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                let mut publication = raw
                    .as_deref()
                    .map(serde_json::from_str::<ProfilePublication>)
                    .transpose()?;
                let friends: bool = db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM members a JOIN members b USING(room) JOIN rooms r ON r.id=a.room
                     WHERE a.actor=?1 AND b.actor=?2 AND r.closed=0 AND r.is_group=0 AND a.through_seq IS NULL AND b.through_seq IS NULL)",
                    params![id,actor], |row| row.get(0))?;
                if id != &actor
                    && !friends
                    && let Some(profile) = &mut publication
                {
                    profile.device = None;
                }
                json!(publication)
            }
        };
        db.commit()?;
        Ok(value)
    }

    fn check_group_relationship(&self, actor: &Card, action: &Action) -> Result<()> {
        let (proof, authority) = match action {
            Action::Offer {
                terms,
                friendship,
                authority,
                ..
            } if terms.data.room.is_some() => {
                ensure!(
                    actor.data.key == self.owner && terms.data.from == self.owner,
                    "Grup davetini host sahibi açabilir"
                );
                verify(&self.owner, "offer", terms)?;
                let proof = friendship.as_deref().context("Arkadaşlık kanıtı eksik")?;
                proof.check(&terms.data.from, &terms.data.to)?;
                (
                    proof.clone(),
                    authority.clone().context("Arkadaşlık hostu bilinmiyor")?,
                )
            }
            Action::Decide { decision }
                if decision.proof.data.accepted && decision.proof.data.terms.room.is_some() =>
            {
                let terms = &decision.proof.data.terms;
                decision.check(&actor.data.key, terms)?;
                let db = self.db.lock().expect("database lock");
                if request(&db, terms.id)?.is_some_and(|r| r.status == Status::Accepted) {
                    return Ok(());
                }
                let (proof, authority): (String, String) = db
                    .query_row(
                        "SELECT proof,authority FROM friendship_checks WHERE request=?1",
                        [terms.id.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .context("Bu grup daveti güncel doğrulama içermiyor. Yeni davet iste.")?;
                (
                    serde_json::from_str(&proof)?,
                    serde_json::from_str(&authority)?,
                )
            }
            _ => return Ok(()),
        };
        ensure!(
            authority.pin == proof.terms.data.host,
            "Arkadaşlık hostu uyuşmuyor"
        );
        let active = if authority.pin == self.pin {
            self.relationship_active(&proof)?
        } else {
            match crate::transport::Connection::new(authority)?.friendship_active(&proof) {
                Ok(active) => active,
                Err(error)
                    if error
                        .downcast_ref::<crate::transport::RemoteError>()
                        .is_none() =>
                {
                    return Err(AuthorityUnavailable.into());
                }
                Err(error) => {
                    return Err(error.context("Arkadaşlık hostu güncel doğrulama yapamadı"));
                }
            }
        };
        ensure!(active, "Arkadaşlık sona ermiş veya doğrulanamadı");
        Ok(())
    }

    fn relationship_active(&self, proof: &Friendship) -> Result<bool> {
        let terms = &proof.terms.data;
        proof.check(&terms.from, &terms.to)?;
        ensure!(terms.host == self.pin, "Arkadaşlık bu hosta ait değil");
        let db = self.db.lock().expect("database lock");
        if request(&db, terms.id)?
            .and_then(|r| r.friendship())
            .as_ref()
            != Some(proof)
        {
            return Ok(false);
        }
        Ok(!room_settings(&db, terms.id)?.2)
    }

    fn offer(
        &self,
        db: &Connection,
        actor: &Card,
        terms: crate::identity::Signed<Terms>,
        target: Card,
        friendship: Option<&Friendship>,
        code_hash: &str,
    ) -> Result<Value> {
        let t = &terms.data;
        ensure!(
            t.from == actor.data.key
                && t.from == self.owner
                && t.to == target.data.key
                && t.to != t.from
                && t.host == self.pin,
            "İsteği yalnızca sohbet hostu ve doğru hesap açabilir"
        );
        crate::identity::decode::<32>(code_hash)?;
        target.check()?;
        verify(&t.from, "offer", &terms)?;
        ensure!(
            t.expires > now() && t.expires <= now() + INVITE_LIFETIME + 60,
            "Davet süresi geçersiz"
        );
        if let Some(existing) = request(db, t.id)? {
            ensure!(
                existing.terms == terms
                    && existing.to == target
                    && db.query_row(
                        "SELECT code_hash FROM requests WHERE id=?1",
                        [t.id.to_string()],
                        |r| r.get::<_, String>(0)
                    )? == code_hash,
                "İstek kimliği başka bir kodla kullanılmış"
            );
            return Ok(json!(existing));
        }
        let room_name = if let Some(room) = t.room {
            let (owner, group, closed) = room_settings(db, room)?;
            ensure!(
                owner == t.from && group && !closed && !active_member(db, room, &t.to)?,
                "Grup davetine izin verilmiyor"
            );
            friendship
                .context("Önce arkadaşlık isteği kabul edilmeli")?
                .check(&t.from, &t.to)?;
            let pending: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM requests WHERE room=?1 AND recipient=?2 AND result IS NULL AND expires>?3)",
                params![room.to_string(), t.to, now()], |r| r.get(0))?;
            ensure!(!pending, "Bu gruba bekleyen davet var");
            Some(db.query_row(
                "SELECT name FROM rooms WHERE id=?1",
                [room.to_string()],
                |r| r.get(0),
            )?)
        } else {
            let exists: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM requests WHERE room IS NULL AND
                ((sender=?1 AND recipient=?2) OR (sender=?2 AND recipient=?1))
                AND ((result=1 AND EXISTS(SELECT 1 FROM rooms WHERE rooms.id=requests.id AND closed=0))
                     OR (result IS NULL AND expires>?3)))",
                params![t.from, t.to, now()],
                |r| r.get(0),
            )?;
            ensure!(!exists, "Bu hesapla bir sohbet veya bekleyen istek var");
            None
        };
        let offer = Request {
            terms,
            from: actor.clone(),
            to: target,
            room_name,
            status: Status::Pending,
            decision: None,
        };
        let t = &offer.terms.data;
        db.execute("INSERT INTO requests(id,sender,recipient,room,code_hash,expires,data) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![t.id.to_string(),t.from,t.to,t.room.map(|r|r.to_string()),code_hash,t.expires,serde_json::to_string(&offer)?])?;
        Ok(json!(offer))
    }

    fn redeem(
        &self,
        db: &Connection,
        actor: &str,
        id: Uuid,
        code: &str,
        grant: Option<&Grant>,
    ) -> Result<Value> {
        let offer = request(db, id)?.context("İstek bulunamadı")?;
        let target = granted_actor(actor, grant)?;
        if let Some(grant) = grant {
            ensure!(
                grant.proof.data.host == self.pin && grant.proof.data.request == id,
                "Operatör yetkisi bu isteğe ait değil"
            );
        }
        ensure!(
            target == offer.terms.data.to && offer.status != Status::Expired,
            "Davet bu hesaba ait değil veya süresi dolmuş"
        );
        let hash: String = db.query_row(
            "SELECT code_hash FROM requests WHERE id=?1",
            [id.to_string()],
            |r| r.get(0),
        )?;
        ensure!(hash == digest(code), "Geçersiz davet kodu");
        db.execute(
            "UPDATE requests SET redeemed=1 WHERE id=?1",
            [id.to_string()],
        )?;
        Ok(json!(offer))
    }

    fn decide(&self, db: &Connection, actor: &str, decision: Decision) -> Result<Value> {
        let terms = &decision.proof.data.terms;
        let mut offer = request(db, terms.id)?.context("İstek bulunamadı")?;
        decision.check(actor, &offer.terms.data)?;
        if matches!(offer.status, Status::Accepted | Status::Rejected) {
            return Ok(json!(offer));
        }
        let redeemed: bool = db.query_row(
            "SELECT redeemed FROM requests WHERE id=?1",
            [terms.id.to_string()],
            |r| r.get(0),
        )?;
        ensure!(
            redeemed && offer.status == Status::Pending,
            "Davet açılmalı ve süresi geçmemiş olmalı"
        );
        let accepted = decision.proof.data.accepted;
        if accepted {
            remember(db, &offer.to)?;
            if let Some(room) = terms.room {
                let (_, _, closed) = room_settings(db, room)?;
                ensure!(!closed, "Grup kapatılmış");
                add_member(db, room, &terms.to)?;
            } else {
                db.execute(
                    "INSERT INTO rooms(id,name,owner,is_group) VALUES(?1,'',?2,0)",
                    params![terms.id.to_string(), terms.from],
                )?;
                add_member(db, terms.id, &terms.from)?;
                add_member(db, terms.id, &terms.to)?;
            }
        }
        let request_id = terms.id;
        offer.status = if accepted {
            Status::Accepted
        } else {
            Status::Rejected
        };
        offer.decision = Some(decision);
        db.execute(
            "UPDATE requests SET result=?1,data=?2 WHERE id=?3",
            params![
                accepted,
                serde_json::to_string(&offer)?,
                request_id.to_string()
            ],
        )?;
        Ok(json!(offer))
    }

    fn sync(
        &self,
        db: &Connection,
        actor: &str,
        after: i64,
        grants: Vec<Grant>,
        read: BTreeMap<Uuid, i64>,
        avatar: Option<AvatarUpdate>,
    ) -> Result<Value> {
        ensure!(
            after >= 0 && grants.len() <= 20 && read.len() <= 500,
            "Geçersiz eşitleme"
        );
        if let Some(update) = avatar {
            if let Some(data) = update.data {
                AvatarPhoto::decode(&data)?;
                let digest = AvatarPhoto::digest(&data);
                db.execute("INSERT INTO avatars VALUES(?1,?2,?3) ON CONFLICT(actor) DO UPDATE SET digest=excluded.digest,data=excluded.data WHERE avatars.digest<>excluded.digest",
                    params![actor, digest, data])?;
            } else {
                db.execute("DELETE FROM avatars WHERE actor=?1", [actor])?;
            }
        }
        let avatar_index = {
            let mut statement = db.prepare(&format!(
                "SELECT actor,digest FROM avatars WHERE {SHARED_AVATAR}"
            ))?;
            statement
                .query_map([actor], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<BTreeMap<_, _>>>()?
        };
        let high = high_water(db)?;
        let delivered = after.min(high);
        if delivered > 0 {
            db.execute("INSERT INTO delivery VALUES(?1,?2) ON CONFLICT(actor) DO UPDATE SET seq=excluded.seq WHERE delivery.seq < excluded.seq", params![actor, delivered])?;
        }
        for (room, seq) in read {
            if db.query_row(
                &format!(
                    "SELECT EXISTS(SELECT 1 FROM messages WHERE room=?2 AND seq=?3 AND {VISIBLE})"
                ),
                params![actor, room.to_string(), seq],
                |row| row.get::<_, bool>(0),
            )? {
                db.execute("INSERT INTO reads VALUES(?1,?2,?3) ON CONFLICT(actor, room) DO UPDATE SET seq=excluded.seq WHERE reads.seq < excluded.seq",
                    params![actor, room.to_string(), seq])?;
            }
        }
        let receipts = {
            let mut statement = db.prepare(
                "SELECT peer.room,peer.actor,peer.after_seq,COALESCE(d.seq,0),COALESCE(r.seq,0)
                 FROM members peer LEFT JOIN delivery d ON d.actor=peer.actor
                 LEFT JOIN reads r ON r.actor=peer.actor AND r.room=peer.room
                 WHERE peer.actor<>?1 AND peer.through_seq IS NULL AND EXISTS
                 (SELECT 1 FROM members own WHERE own.actor=?1 AND own.room=peer.room AND own.through_seq IS NULL)
                 ORDER BY peer.room,peer.actor",
            )?;
            statement
                .query_map([actor], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                })?
                .map(|row| {
                    let (room, actor, joined_after, delivered, read) = row?;
                    Ok(Receipt {
                        room: Uuid::parse_str(&room)?,
                        actor,
                        joined_after,
                        delivered,
                        read,
                    })
                })
                .collect::<Result<Vec<_>>>()?
        };
        let rooms = {
            let mut statement = db.prepare("SELECT DISTINCT room FROM members WHERE actor=?1")?;
            statement
                .query_map([actor], |row| row.get::<_, String>(0))?
                .map(|row| room(db, actor, Uuid::parse_str(&row?)?))
                .collect::<Result<Vec<_>>>()?
        };
        let mut requests = {
            let mut statement = db.prepare("SELECT data FROM requests WHERE sender=?1 OR (recipient=?1 AND redeemed=1) ORDER BY rowid DESC LIMIT 500")?;
            statement
                .query_map([actor], |row| row.get::<_, String>(0))?
                .map(|row| read_request(&row?))
                .collect::<Result<Vec<_>>>()?
        };
        for grant in &grants {
            let delegate = granted_actor(actor, Some(grant))?;
            ensure!(
                grant.proof.data.host == self.pin,
                "Operatör yetkisi bu hosta ait değil"
            );
            let id = grant.proof.data.request;
            if let Some(raw) = db
                .query_row(
                    "SELECT data FROM requests WHERE id=?1 AND recipient=?2 AND redeemed=1",
                    params![id.to_string(), delegate],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                && !requests.iter().any(|request| request.terms.data.id == id)
            {
                requests.push(read_request(&raw)?);
            }
        }
        let ids = message_ids(
            db,
            &format!(
                "SELECT seq FROM messages WHERE {VISIBLE} AND seq>?2 ORDER BY seq LIMIT {PAGE}"
            ),
            params![actor, after],
        )?;
        let cursor = if ids.len() == PAGE {
            ids[PAGE - 1]
        } else {
            high.max(after)
        };
        let messages = ids
            .into_iter()
            .map(|seq| message(db, actor, seq))
            .collect::<Result<Vec<_>>>()?;
        let introductions = if actor == self.owner {
            let mut statement = db.prepare(
                "SELECT ticket FROM introductions WHERE expires>?1 ORDER BY rowid DESC LIMIT 250",
            )?;
            statement
                .query_map([now()], |row| row.get::<_, String>(0))?
                .map(|row| Ok(serde_json::from_str(&row?)?))
                .collect::<Result<Vec<_>>>()?
        } else {
            vec![]
        };
        let task_activity = {
            let mut statement = db.prepare(
                "SELECT a.data FROM task_activity a JOIN rooms r ON r.id=a.room
                 WHERE a.recipient=?1 AND a.updated>?2 AND r.closed=0 AND EXISTS
                   (SELECT 1 FROM members m WHERE m.room=a.room AND m.actor=a.recipient AND m.through_seq IS NULL)
                   AND EXISTS (SELECT 1 FROM members m WHERE m.room=a.room AND m.actor=a.agent AND m.through_seq IS NULL)
                 ORDER BY a.updated DESC, a.source DESC LIMIT 64",
            )?;
            statement
                .query_map(params![actor, now() - 75], |row| row.get::<_, String>(0))?
                .map(|row| Ok(serde_json::from_str(&row?)?))
                .collect::<Result<Vec<_>>>()?
        };
        let withdrawn_messages = {
            let mut statement = db.prepare(&format!(
                "SELECT seq FROM messages WHERE withdrawn=1 AND {VISIBLE} ORDER BY seq"
            ))?;
            statement
                .query_map([actor], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<i64>>>()?
        };
        Ok(json!(Sync {
            cursor,
            rooms,
            requests,
            messages,
            withdrawn_messages,
            supports_message_modes: true,
            receipts,
            supports_attachments: true,
            avatar_index,
            introductions,
            max_file_bytes: MAX_BINARY_ATTACHMENT,
            agent_binary_files: true,
            supports_task_activity: true,
            task_activity,
        }))
    }
}

fn ensure_message_columns(db: &Connection) -> Result<()> {
    let columns = {
        let mut statement = db.prepare("PRAGMA table_info(messages)")?;
        statement
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<std::collections::BTreeSet<_>>>()?
    };
    if !columns.contains("sent_ms") {
        db.execute(
            "ALTER TABLE messages ADD COLUMN sent_ms INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !columns.contains("undo_deadline_ms") {
        db.execute(
            "ALTER TABLE messages ADD COLUMN undo_deadline_ms INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !columns.contains("withdrawn") {
        db.execute(
            "ALTER TABLE messages ADD COLUMN withdrawn INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    db.execute(
        "UPDATE messages SET sent_ms=time*1000,undo_deadline_ms=time*1000 WHERE sent_ms=0",
        [],
    )?;
    Ok(())
}

fn is_withdrawn(db: &Connection, seq: i64) -> Result<bool> {
    Ok(db.query_row(
        "SELECT withdrawn FROM messages WHERE seq=?1",
        [seq],
        |row| row.get(0),
    )?)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn publish_task_activity(db: &Connection, agent: &str, activity: TaskActivity) -> Result<()> {
    ensure!(
        activity.source > 0
            && activity
                .plan
                .is_none_or(|[done, total]| total > 0 && done <= total),
        "Geçersiz görev durumu"
    );
    let room = activity.room.to_string();
    let (recipient, card): (String, String) = db.query_row(
        "SELECT s.actor, p.card FROM messages s JOIN rooms r ON r.id=s.room JOIN profiles p ON p.id=s.actor
         WHERE s.seq=?1 AND s.room=?2 AND r.is_group=0 AND r.closed=0 AND s.actor<>?3
           AND EXISTS (SELECT 1 FROM members m WHERE m.room=s.room AND m.actor=?3 AND s.seq>m.after_seq AND m.through_seq IS NULL)
           AND EXISTS (SELECT 1 FROM members m WHERE m.room=s.room AND m.actor=s.actor AND s.seq>m.after_seq AND m.through_seq IS NULL)",
        params![activity.source, room, agent],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?.context("Görev mesajına erişim yok")?;
    ensure!(
        serde_json::from_str::<Card>(&card)?.data.role == Role::Human,
        "Görev durumu insan hesabına iletilir"
    );
    let timestamp = now();
    db.execute(
        "DELETE FROM task_activity WHERE updated<=?1",
        [timestamp - 75],
    )?;
    db.execute(
        "INSERT INTO task_activity(source,room,agent,recipient,updated,data) VALUES(?1,?2,?3,?4,?5,?6)
         ON CONFLICT(source) DO UPDATE SET updated=excluded.updated,data=excluded.data",
        params![activity.source, room, agent, recipient, timestamp, serde_json::to_string(&activity)?],
    )?;
    Ok(())
}

fn remember(db: &Connection, card: &Card) -> Result<()> {
    let saved: Option<String> = db
        .query_row(
            "SELECT card FROM profiles WHERE id=?1",
            [&card.data.key],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(saved) = saved {
        ensure!(
            serde_json::from_str::<Card>(&saved)? == *card,
            "Hesap kartı değiştirilemez"
        );
    } else {
        db.execute(
            "INSERT INTO profiles VALUES(?1,?2)",
            params![card.data.key, serde_json::to_string(card)?],
        )?;
    }
    Ok(())
}

fn profile(db: &Connection, id: &str) -> Result<Card> {
    Ok(serde_json::from_str(&db.query_row(
        "SELECT card FROM profiles WHERE id=?1",
        [id],
        |r| r.get::<_, String>(0),
    )?)?)
}

fn clean(text: &str, limit: usize) -> Result<String> {
    let text = text.trim();
    ensure!(
        !text.is_empty()
            && text.chars().count() <= limit
            && !text
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t'),
        "Geçersiz veya çok uzun metin"
    );
    Ok(text.into())
}

fn granted_actor<'a>(actor: &'a str, grant: Option<&'a Grant>) -> Result<&'a str> {
    if let Some(grant) = grant {
        grant.check()?;
        ensure!(
            grant.operator.data.key == actor,
            "Operatör yetkisi bu hesaba ait değil"
        );
        Ok(&grant.agent.data.key)
    } else {
        Ok(actor)
    }
}

fn read_request(raw: &str) -> Result<Request> {
    let mut record: Request = serde_json::from_str(raw)?;
    if record.status == Status::Pending && record.terms.data.expires <= now() {
        record.status = Status::Expired;
    }
    Ok(record)
}

fn request(db: &Connection, id: Uuid) -> Result<Option<Request>> {
    let raw: Option<String> = db
        .query_row(
            "SELECT data FROM requests WHERE id=?1",
            [id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    raw.as_deref().map(read_request).transpose()
}

fn high_water(db: &Connection) -> Result<i64> {
    Ok(
        db.query_row("SELECT COALESCE(MAX(seq),0) FROM messages", [], |r| {
            r.get(0)
        })?,
    )
}

fn active_member(db: &Connection, room: Uuid, id: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM members WHERE room=?1 AND actor=?2 AND through_seq IS NULL)",
        params![room.to_string(), id],
        |r| r.get(0),
    )?)
}

fn ever_member(db: &Connection, room: Uuid, id: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM members WHERE room=?1 AND actor=?2)",
        params![room.to_string(), id],
        |r| r.get(0),
    )?)
}

fn room_settings(db: &Connection, room: Uuid) -> Result<(String, bool, bool)> {
    db.query_row(
        "SELECT owner,is_group,closed FROM rooms WHERE id=?1",
        [room.to_string()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()?
    .context("Sohbet bulunamadı")
}

fn room(db: &Connection, viewer: &str, id: Uuid) -> Result<Room> {
    let (owner, group, closed) = room_settings(db, id)?;
    let active = active_member(db, id, viewer)?;
    let mut statement = db.prepare("SELECT p.card FROM members m JOIN profiles p ON p.id=m.actor WHERE room=?1 AND through_seq IS NULL ORDER BY p.id")?;
    let mut members: Vec<Card> = statement
        .query_map([id.to_string()], |r| r.get::<_, String>(0))?
        .map(|row| Ok(serde_json::from_str(&row?)?))
        .collect::<Result<_>>()?;
    let name = if group {
        db.query_row(
            "SELECT name FROM rooms WHERE id=?1",
            [id.to_string()],
            |r| r.get(0),
        )?
    } else {
        members
            .iter()
            .find(|card| card.data.key != viewer)
            .context("Sohbet katılımcısı bulunamadı")?
            .data
            .name
            .clone()
    };
    if !active {
        members.clear();
    }
    Ok(Room {
        id,
        name,
        owner,
        group,
        closed,
        active,
        members,
    })
}

fn add_member(db: &Connection, room: Uuid, actor: &str) -> Result<()> {
    let count: u32 = db.query_row(
        "SELECT COUNT(*) FROM members WHERE room=?1 AND through_seq IS NULL",
        [room.to_string()],
        |r| r.get(0),
    )?;
    ensure!(count < 50, "Grup en fazla 50 üyeli olabilir");
    db.execute(
        "INSERT INTO members(room,actor,after_seq) VALUES(?1,?2,?3)",
        params![room.to_string(), actor, high_water(db)?],
    )?;
    Ok(())
}

fn finish_membership(db: &Connection, room: Uuid, actor: &str) -> Result<()> {
    db.execute(
        "UPDATE members SET through_seq=?1 WHERE room=?2 AND actor=?3 AND through_seq IS NULL",
        params![high_water(db)?, room.to_string(), actor],
    )?;
    Ok(())
}

fn message_ids(db: &Connection, sql: &str, args: impl rusqlite::Params) -> Result<Vec<i64>> {
    let mut stmt = db.prepare(sql)?;
    Ok(stmt
        .query_map(args, |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

fn visible(db: &Connection, actor: &str, seq: i64) -> Result<bool> {
    Ok(db.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM messages WHERE seq=?2 AND {VISIBLE})"),
        params![actor, seq],
        |r| r.get(0),
    )?)
}

fn attachment_info(db: &Connection, seq: i64) -> Result<Option<AttachmentInfo>> {
    let row = db
        .query_row(
            "SELECT a.name,a.bytes,p.preview,EXISTS(SELECT 1 FROM binary_files b WHERE b.seq=a.seq) FROM attachments a LEFT JOIN attachment_previews p ON p.seq=a.seq WHERE a.seq=?1",
            [seq],
            |row| Ok((row.get::<_, String>(0)?, row.get(1)?, row.get::<_, Option<String>>(2)?, row.get(3)?)),
        )
        .optional()?;
    row.map(|(name, bytes, preview, binary)| {
        Ok(AttachmentInfo {
            name,
            bytes,
            preview: preview.as_deref().map(serde_json::from_str).transpose()?,
            binary,
        })
    })
    .transpose()
}

fn attachment_preview(bytes: &[u8]) -> Result<Option<AttachmentPreview>> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    if !matches!(
        reader.format(),
        Some(image::ImageFormat::Jpeg | image::ImageFormat::Png)
    ) {
        return Ok(None);
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(2048);
    limits.max_image_height = Some(2048);
    limits.max_alloc = Some(24 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode()?;
    let edge = image
        .width()
        .max(image.height())
        .min(AttachmentPreview::MAX_DIMENSION);
    let thumbnail = image.thumbnail(edge, edge).to_rgb8();
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 77).encode_image(&thumbnail)?;
    let data = encode(bytes);
    Ok(
        (data.len() <= MAX_AVATAR_DATA).then_some(AttachmentPreview {
            width: thumbnail.width(),
            height: thumbnail.height(),
            data,
        }),
    )
}

fn message(db: &Connection, viewer: &str, seq: i64) -> Result<Message> {
    let (id, author, raw, sent_at, reply, root, sent_ms, undo_until_ms, withdrawn): (
        String,
        String,
        String,
        i64,
        Option<i64>,
        Option<i64>,
        i64,
        i64,
        bool,
    ) = db.query_row(
        "SELECT room,actor,payload,time,reply,root,sent_ms,undo_deadline_ms,withdrawn FROM messages WHERE seq=?1",
        [seq],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
            ))
        },
    )?;
    let draft: Draft = serde_json::from_str(&raw)?;
    let mut quote = None;
    if !withdrawn
        && let Some(source) = reply.filter(|source| *source != seq)
        && visible(db, viewer, source)?
        && !is_withdrawn(db, source)?
    {
        let (author, payload): (String, String) = db.query_row(
            "SELECT actor,payload FROM messages WHERE seq=?1",
            [source],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let original: Draft = serde_json::from_str(&payload)?;
        quote = Some(Quote {
            id: source,
            author: profile(db, &author)?,
            text: if original.text.is_empty() {
                attachment_info(db, source)?.map_or_else(String::new, |file| file.name)
            } else {
                original.text.chars().take(240).collect()
            },
        });
    }
    let withdrawn_attachment = if withdrawn {
        None
    } else {
        attachment_info(db, seq)?
    };
    Ok(Message {
        seq,
        room: Uuid::parse_str(&id)?,
        author: profile(db, &author)?,
        client_id: draft.id,
        text: if withdrawn { String::new() } else { draft.text },
        sent_at,
        reply: quote,
        root,
        invitation: if withdrawn { None } else { draft.invitation },
        withdrawn,
        undo_until_ms: if sent_ms == 0 { 0 } else { undo_until_ms },
        mode: if withdrawn { None } else { draft.mode },
        attachment: withdrawn_attachment,
    })
}

fn send(
    db: &Connection,
    actor: &Card,
    mut draft: Draft,
    binary: Option<(String, usize, String)>,
) -> Result<Message> {
    let author = &actor.data.key;
    let attachment = draft.attachment.take();
    let file_bytes = if let Some(file) = &attachment {
        ensure!(
            draft.invitation.is_none(),
            "Davetle birlikte dosya gönderilemez"
        );
        Some(file.bytes()?)
    } else {
        None
    };
    draft.text = if (attachment.is_some() || binary.is_some()) && draft.text.trim().is_empty() {
        String::new()
    } else {
        clean(&draft.text, MAX_TEXT)?
    };
    if let Some(mode) = &draft.mode {
        ensure!(
            actor.data.role == Role::Human,
            "Otomasyon modunu yalnızca insan hesabı seçebilir"
        );
        ensure!(
            !draft.text.trim().is_empty(),
            "Otomasyon yönergesi boş olamaz"
        );
        if let MessageMode::Loop { interval_seconds } = mode {
            ensure!(
                (10..=30 * 24 * 60 * 60).contains(interval_seconds),
                "Döngü aralığı 10 saniye ile 30 gün arasında olmalı"
            );
        }
    }
    let payload = serde_json::to_string(&draft)?;
    let existing: Option<(i64, String)> = db
        .query_row(
            "SELECT seq,payload FROM messages WHERE actor=?1 AND client_id=?2",
            params![author, draft.id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((seq, original)) = existing {
        if is_withdrawn(db, seq)? {
            return message(db, author, seq);
        }
        if let Some(expected) = binary {
            let saved = db.query_row("SELECT a.name,a.bytes,b.hash FROM attachments a JOIN binary_files b USING(seq) WHERE seq=?1",
                [seq], |r| Ok((r.get::<_, String>(0)?, r.get::<_, usize>(1)?, r.get::<_, String>(2)?))).optional()?;
            ensure!(
                original == payload && saved == Some(expected),
                "Mesaj kimliği başka içerikle kullanılmış"
            );
            return message(db, author, seq);
        }
        let original_attachment = db
            .query_row(
                "SELECT name,data FROM attachments WHERE seq=?1",
                [seq],
                |row| {
                    Ok(Attachment {
                        name: row.get(0)?,
                        data: row.get(1)?,
                    })
                },
            )
            .optional()?;
        ensure!(
            original == payload && original_attachment == attachment,
            "Mesaj kimliği başka içerikle kullanılmış"
        );
        return message(db, author, seq);
    }
    let (_, group, closed) = room_settings(db, draft.room)?;
    ensure!(
        !closed && active_member(db, draft.room, author)?,
        "Sohbete gönderme yetkisi yok"
    );
    if binary.is_some() && actor.data.role == Role::Agent {
        let source = draft
            .reply
            .context("Ajan dosyası bir insan mesajına yanıt olmalı")?;
        ensure!(
            !group
                && visible(db, author, source)?
                && message(db, author, source)?.author.data.role == Role::Human,
            "Ajan dosyaları bire bir insan sohbetlerinde gönderilebilir"
        );
    }
    if let Some(ticket) = &draft.invitation {
        ensure!(
            !group && actor.data.role == Role::Human && ticket.grant.is_none(),
            "Davet yalnızca bire bir sohbetten gönderilebilir"
        );
        ticket.host.check()?;
    }
    let mut root = None;
    if let Some(source) = draft.reply {
        let room: String = db
            .query_row("SELECT room FROM messages WHERE seq=?1", [source], |r| {
                r.get(0)
            })
            .optional()?
            .context("Kaynak mesaj bulunamadı")?;
        ensure!(
            room == draft.room.to_string() && visible(db, author, source)?,
            "Kaynak mesaja erişim yok"
        );
        if actor.data.role == Role::Agent {
            let trigger = message(db, author, source)?;
            ensure!(
                trigger.author.data.key != *author,
                "Ajan kendi mesajını yanıtlayamaz"
            );
            if trigger.author.data.role == Role::Agent {
                ensure!(
                    mentions(&trigger.text, &actor.data.handle)
                        || trigger
                            .reply
                            .as_ref()
                            .is_some_and(|q| q.author.data.key == *author),
                    "Ajanlar arası yanıt açık bir hitap gerektirir"
                );
            }
            let chain = trigger.root.unwrap_or(trigger.seq);
            let previous: Option<i64> = db
                .query_row(
                    "SELECT seq FROM messages WHERE actor=?1 AND root=?2",
                    params![author, chain],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(previous) = previous {
                return message(db, author, previous);
            }
            let total: u32 = db.query_row(
                "SELECT COUNT(*) FROM messages WHERE root=?1",
                [chain],
                |r| r.get(0),
            )?;
            ensure!(total < 3, "Bu konuşma zinciri tamamlandı");
            root = Some(chain);
        }
    }
    let sent_ms = now_ms();
    db.execute("INSERT INTO messages(room,actor,client_id,time,payload,reply,root,sent_ms,undo_deadline_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![draft.room.to_string(),author,draft.id.to_string(),sent_ms / 1000,payload,draft.reply,root,sent_ms,sent_ms + 500])?;
    let seq = db.last_insert_rowid();
    if let Some(file) = attachment {
        db.execute(
            "INSERT INTO attachments VALUES(?1,?2,?3,?4)",
            params![seq, file.name, file.data, file.info().bytes],
        )?;
        if let Some(preview) = file_bytes
            .as_deref()
            .and_then(|bytes| attachment_preview(bytes).ok().flatten())
        {
            db.execute(
                "INSERT INTO attachment_previews VALUES(?1,?2)",
                params![seq, serde_json::to_string(&preview)?],
            )?;
        }
    }
    if let Some((name, bytes, hash)) = binary {
        let mut statement = db.prepare(
            "SELECT offset,data FROM upload_chunks WHERE actor=?1 AND id=?2 ORDER BY offset",
        )?;
        let chunks = statement.query_map(params![author, draft.id.to_string()], |row| {
            Ok((row.get::<_, usize>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        let mut content = Vec::with_capacity(bytes);
        for chunk in chunks {
            let (offset, chunk) = chunk?;
            ensure!(
                offset == content.len() && offset + chunk.len() <= bytes,
                "Dosya aktarımı tamamlanmadı"
            );
            content.extend(chunk);
        }
        ensure!(
            content.len() == bytes && digest(&content) == hash,
            "Dosya doğrulanamadı"
        );
        db.execute(
            "INSERT INTO attachments VALUES(?1,?2,'',?3)",
            params![seq, name, bytes],
        )?;
        db.execute(
            "INSERT INTO binary_files VALUES(?1,?2,?3)",
            params![seq, hash, content],
        )?;
        if let Some(preview) = attachment_preview(&content).ok().flatten() {
            db.execute(
                "INSERT INTO attachment_previews VALUES(?1,?2)",
                params![seq, serde_json::to_string(&preview)?],
            )?;
        }
    }
    message(db, author, seq)
}

pub fn mentions(text: &str, handle: &str) -> bool {
    mention_ranges(text).any(|range| text[range.start + 1..range.end].eq_ignore_ascii_case(handle))
}

pub(crate) fn mention_ranges(text: &str) -> impl Iterator<Item = std::ops::Range<usize>> + '_ {
    text.match_indices('@').filter_map(|(start, _)| {
        if text[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            return None;
        }
        let handle = text[start + 1..]
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .next()?;
        (!handle.is_empty()).then_some(start..start + 1 + handle.len())
    })
}

struct Failure(StatusCode, String);

#[derive(Debug)]
struct AuthorityUnavailable;

impl std::fmt::Display for AuthorityUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Arkadaşlık hostundan güncel doğrulama bekleniyor")
    }
}

impl std::error::Error for AuthorityUnavailable {}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

fn invalid(error: anyhow::Error) -> Failure {
    if error.downcast_ref::<AuthorityUnavailable>().is_some() {
        Failure(StatusCode::SERVICE_UNAVAILABLE, error.to_string())
    } else if error.downcast_ref::<rusqlite::Error>().is_some() {
        eprintln!("ChatOid host: {error:#}");
        Failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Host veritabanı hatası".into(),
        )
    } else {
        Failure(StatusCode::BAD_REQUEST, error.to_string())
    }
}

async fn blocking<T>(
    callback: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<Json<T>, Failure>
where
    T: Serialize + Send + 'static,
{
    tokio::task::spawn_blocking(callback)
        .await
        .map_err(|e| invalid(anyhow!(e)))?
        .map(Json)
        .map_err(invalid)
}

async fn challenge(State(host): State<Arc<Host>>) -> Result<Json<String>, Failure> {
    blocking(move || host.challenge()).await
}

async fn login(
    State(host): State<Arc<Host>>,
    Json(login): Json<SignIn>,
) -> Result<Json<String>, Failure> {
    blocking(move || host.login(login)).await
}

async fn friendship(
    State(host): State<Arc<Host>>,
    Json(proof): Json<Friendship>,
) -> Result<Json<bool>, Failure> {
    blocking(move || host.relationship_active(&proof)).await
}

async fn upload(
    State(host): State<Arc<Host>>,
    RoutePath((id, offset)): RoutePath<(Uuid, usize)>,
    headers: HeaderMap,
    chunk: Bytes,
) -> Result<Json<()>, Failure> {
    let actor = host
        .auth(&headers)
        .ok_or_else(|| Failure(StatusCode::UNAUTHORIZED, "Oturum geçersiz".into()))?;
    blocking(move || {
        ensure!(
            offset.is_multiple_of(FILE_CHUNK)
                && (1..=FILE_CHUNK).contains(&chunk.len())
                && offset <= MAX_BINARY_ATTACHMENT - chunk.len(),
            "Geçersiz dosya parçası"
        );
        let db = host.db.lock().expect("database lock");
        if actor.data.role == Role::Agent {
            let direct: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM members m JOIN rooms r ON r.id=m.room WHERE m.actor=?1 AND m.through_seq IS NULL AND r.is_group=0 AND r.closed=0)",
                [&actor.data.key], |row| row.get(0),
            )?;
            ensure!(direct, "Ajan dosyası için açık bire bir sohbet gerekli");
        }
        let id = id.to_string();
        let published: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE actor=?1 AND client_id=?2)",
            params![actor.data.key, id],
            |row| row.get(0),
        )?;
        if published {
            return Ok(());
        }
        db.execute("DELETE FROM upload_chunks WHERE updated<?1", [now() - 3600])?;
        let previous: Option<Vec<u8>> = db
            .query_row(
                "SELECT data FROM upload_chunks WHERE actor=?1 AND id=?2 AND offset=?3",
                params![actor.data.key, id, offset],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(previous) = previous {
            ensure!(previous == chunk.as_ref(), "Yükleme parçası değişti");
            return Ok(());
        }
        let total: usize = db.query_row(
            "SELECT COALESCE(SUM(length(data)),0) FROM upload_chunks",
            [],
            |row| row.get(0),
        )?;
        let owned: usize = db.query_row(
            "SELECT COALESCE(SUM(length(data)),0) FROM upload_chunks WHERE actor=?1",
            [&actor.data.key],
            |row| row.get(0),
        )?;
        ensure!(
            total + chunk.len() <= 8 * MAX_BINARY_ATTACHMENT
                && owned + chunk.len() <= 2 * MAX_BINARY_ATTACHMENT,
            "Host dosya kuyruğu dolu"
        );
        db.execute(
            "INSERT INTO upload_chunks VALUES(?1,?2,?3,?4,?5)",
            params![actor.data.key, id, offset, chunk.as_ref(), now()],
        )?;
        Ok(())
    })
    .await
}

async fn download_binary(
    State(host): State<Arc<Host>>,
    RoutePath((seq, offset)): RoutePath<(i64, usize)>,
    headers: HeaderMap,
) -> Result<Bytes, Failure> {
    let actor = host
        .auth(&headers)
        .ok_or_else(|| Failure(StatusCode::UNAUTHORIZED, "Oturum geçersiz".into()))?;
    blocking(move || {
        let db = host.db.lock().expect("database lock");
        ensure!(
            offset < MAX_BINARY_ATTACHMENT && visible(&db, &actor.data.key, seq)?,
            "Dosyaya erişim yok"
        );
        let bytes: Vec<u8> = db
            .query_row(
                "SELECT substr(data,?2,?3) FROM binary_files WHERE seq=?1",
                params![seq, offset + 1, FILE_CHUNK],
                |row| row.get(0),
            )
            .context("Dosya bulunamadı")?;
        Ok(bytes)
    })
    .await
    .map(|Json(bytes)| Bytes::from(bytes))
}

async fn action(
    State(host): State<Arc<Host>>,
    headers: HeaderMap,
    Json(action): Json<Action>,
) -> Result<Json<Value>, Failure> {
    let actor = host
        .auth(&headers)
        .ok_or_else(|| Failure(StatusCode::UNAUTHORIZED, "Oturum geçersiz".into()))?;
    blocking(move || host.apply(&actor, action)).await
}
