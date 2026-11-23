use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    identity::{Card, Role, encode, random, verify},
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
  UNIQUE(actor, client_id));
CREATE INDEX IF NOT EXISTS room_messages ON messages(room, seq);
CREATE UNIQUE INDEX IF NOT EXISTS source_agent_reply ON messages(actor, reply) WHERE root IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS root_agent_reply ON messages(actor, root) WHERE root IS NOT NULL;
";

const VISIBLE: &str = "
EXISTS (SELECT 1 FROM members m WHERE m.room = messages.room AND m.actor = ?1
  AND messages.seq > m.after_seq AND (m.through_seq IS NULL OR messages.seq <= m.through_seq))";

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
            .layer(DefaultBodyLimit::max(64 * 1024))
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
        let mut connection = self.db.lock().expect("database lock");
        let db = connection.transaction()?;
        let id = &actor.data.key;
        let value = match action {
            Action::Sync { after, grants } => self.sync(&db, id, after, grants)?,
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
            } => self.offer(&db, actor, terms, target, friendship.as_deref(), &code_hash)?,
            Action::Redeem {
                id: request,
                code,
                grant,
            } => self.redeem(&db, id, request, &code, grant.as_ref())?,
            Action::Decide { decision } => self.decide(&db, id, decision)?,
            Action::Send { draft } => serde_json::to_value(send(&db, actor, draft)?)?,
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
        };
        db.commit()?;
        Ok(value)
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
                AND (result=1 OR (result IS NULL AND expires>?3)))",
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

    fn sync(&self, db: &Connection, actor: &str, after: i64, grants: Vec<Grant>) -> Result<Value> {
        ensure!(after >= 0 && grants.len() <= 20, "Geçersiz eşitleme");
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
            high_water(db)?.max(after)
        };
        let messages = ids
            .into_iter()
            .map(|seq| message(db, actor, seq))
            .collect::<Result<Vec<_>>>()?;
        Ok(json!(Sync {
            cursor,
            rooms,
            requests,
            messages
        }))
    }
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

fn message(db: &Connection, viewer: &str, seq: i64) -> Result<Message> {
    let (id, author, raw, sent_at, reply, root): (
        String,
        String,
        String,
        i64,
        Option<i64>,
        Option<i64>,
    ) = db.query_row(
        "SELECT room,actor,payload,time,reply,root FROM messages WHERE seq=?1",
        [seq],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        },
    )?;
    let draft: Draft = serde_json::from_str(&raw)?;
    let mut quote = None;
    if let Some(source) = reply.filter(|source| *source != seq)
        && visible(db, viewer, source)?
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
            text: original.text.chars().take(240).collect(),
        });
    }
    Ok(Message {
        seq,
        room: Uuid::parse_str(&id)?,
        author: profile(db, &author)?,
        client_id: draft.id,
        text: draft.text,
        sent_at,
        reply: quote,
        root,
        invitation: draft.invitation,
    })
}

fn send(db: &Connection, actor: &Card, mut draft: Draft) -> Result<Message> {
    let author = &actor.data.key;
    draft.text = clean(&draft.text, MAX_TEXT)?;
    let payload = serde_json::to_string(&draft)?;
    let existing: Option<(i64, String)> = db
        .query_row(
            "SELECT seq,payload FROM messages WHERE actor=?1 AND client_id=?2",
            params![author, draft.id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((seq, original)) = existing {
        ensure!(
            original == payload,
            "Mesaj kimliği başka içerikle kullanılmış"
        );
        return message(db, author, seq);
    }
    let (_, group, closed) = room_settings(db, draft.room)?;
    ensure!(
        !closed && active_member(db, draft.room, author)?,
        "Sohbete gönderme yetkisi yok"
    );
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
    db.execute("INSERT INTO messages(room,actor,client_id,time,payload,reply,root) VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![draft.room.to_string(),author,draft.id.to_string(),now(),payload,draft.reply,root])?;
    message(db, author, db.last_insert_rowid())
}

pub fn mentions(text: &str, handle: &str) -> bool {
    text.match_indices('@').any(|(i, _)| {
        !text[..i]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
            && text[i + 1..]
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .next()
                .is_some_and(|word| word.eq_ignore_ascii_case(handle))
    })
}

struct Failure(StatusCode, String);

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

fn invalid(error: anyhow::Error) -> Failure {
    if error.downcast_ref::<rusqlite::Error>().is_some() {
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
