use std::{
    collections::BTreeMap,
    net::SocketAddr,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::identity::{AgentProfile, Card, Identity, Role, Signed, decode, encode, verify};

pub const VERSION: u8 = 1;
pub const PAGE: usize = 200;
pub const INVITE_LIFETIME: i64 = 7 * 24 * 3600;
pub const MAX_TEXT: usize = 4000;
pub const MAX_ATTACHMENT: usize = 1024 * 1024;
pub const MAX_BINARY_ATTACHMENT: usize = 32 * 1024 * 1024;
pub const FILE_CHUNK: usize = 512 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceSummary {
    pub system: String,
    pub architecture: String,
    pub processor: String,
    pub memory: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicProfile {
    pub display_name: String,
    pub agent: Option<AgentProfile>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfilePublication {
    pub profile: Signed<PublicProfile>,
    pub device: Option<Signed<DeviceSummary>>,
}
pub const AVATAR_PX: u32 = 80;
pub const AVATAR_PRESETS: [[u8; 3]; 12] = [
    [133, 229, 197],
    [153, 193, 242],
    [219, 171, 241],
    [244, 194, 136],
    [244, 157, 177],
    [192, 222, 143],
    [114, 211, 230],
    [184, 175, 250],
    [245, 211, 135],
    [250, 166, 142],
    [150, 216, 180],
    [238, 167, 216],
];
pub const MAX_AVATAR_DATA: usize = 24 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AvatarUpdate {
    pub data: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AvatarPhoto {
    pub digest: String,
    pub data: String,
}

impl AvatarPhoto {
    pub fn digest(data: &str) -> String {
        encode(Sha256::digest(data.as_bytes()))
    }

    pub fn decode(data: &str) -> Result<image::DynamicImage> {
        ensure!(
            data.len() <= MAX_AVATAR_DATA,
            "Profil fotoğrafı boyutu aşıldı"
        );
        let bytes = URL_SAFE_NO_PAD.decode(data)?;
        let mut reader =
            image::ImageReader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Jpeg);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(AVATAR_PX);
        limits.max_image_height = Some(AVATAR_PX);
        limits.max_alloc = Some(1024 * 1024);
        reader.limits(limits);
        let image = reader.decode()?;
        ensure!(
            image.width() == AVATAR_PX && image.height() == AVATAR_PX,
            "Profil fotoğrafı boyutları geçersiz"
        );
        Ok(image)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    pub data: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AttachmentInfo {
    pub name: String,
    pub bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<AttachmentPreview>,
    #[serde(default)]
    pub binary: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AttachmentPreview {
    pub width: u32,
    pub height: u32,
    pub data: String,
}

impl AttachmentPreview {
    pub const MAX_DIMENSION: u32 = 180;

    pub fn decode(&self) -> Result<image::DynamicImage> {
        ensure!(self.data.len() <= MAX_AVATAR_DATA, "Önizleme boyutu aşıldı");
        let bytes = URL_SAFE_NO_PAD.decode(&self.data)?;
        let mut reader =
            image::ImageReader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Jpeg);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(Self::MAX_DIMENSION);
        limits.max_image_height = Some(Self::MAX_DIMENSION);
        limits.max_alloc = Some(1024 * 1024);
        reader.limits(limits);
        let image = reader.decode()?;
        ensure!(
            image.width() == self.width && image.height() == self.height,
            "Önizleme boyutları geçersiz"
        );
        Ok(image)
    }
}

pub fn check_filename(name: &str) -> Result<()> {
    ensure!(
        !name.trim().is_empty()
            && name.chars().count() <= 120
            && name != "."
            && name != ".."
            && !name
                .chars()
                .any(|c| c.is_control() || r#"/\:<>|"?*"#.contains(c))
            && !name.ends_with(['.', ' ']),
        "Geçersiz dosya adı"
    );
    Ok(())
}

impl Attachment {
    pub fn bytes(&self) -> Result<Vec<u8>> {
        ensure!(
            !self.name.trim().is_empty()
                && self.name.chars().count() <= 120
                && self.name != "."
                && self.name != ".."
                && !self
                    .name
                    .chars()
                    .any(|c| c.is_control() || "/\\:<>|\"?*".contains(c))
                && !self.name.ends_with(['.', ' ']),
            "Geçersiz dosya adı"
        );
        ensure!(
            self.data.len() <= MAX_ATTACHMENT.div_ceil(3) * 4,
            "Dosya 1 MiB sınırını aşıyor"
        );
        let bytes = URL_SAFE_NO_PAD.decode(&self.data)?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_ATTACHMENT,
            "Dosya boyutu 1 bayt ile 1 MiB arasında olmalı"
        );
        Ok(bytes)
    }

    pub fn info(&self) -> AttachmentInfo {
        AttachmentInfo {
            name: self.name.clone(),
            bytes: self.data.len() * 3 / 4,
            preview: None,
            binary: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub room: Uuid,
    pub actor: String,
    pub joined_after: i64,
    pub delivered: i64,
    pub read: i64,
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostAddress {
    pub socket: SocketAddr,
    pub pin: String,
}

impl HostAddress {
    pub fn check(&self) -> Result<()> {
        ensure!(
            self.socket.port() != 0 && allowed_ip(self.socket.ip()),
            "Host adresi yerel ağ veya VPN üzerinde olmalı"
        );
        decode::<32>(&self.pin)?;
        Ok(())
    }
}

pub fn allowed_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            ip.is_private() || ip.is_loopback() || ip.is_link_local() || {
                let octets = ip.octets();
                octets[0] == 100 && (64..=127).contains(&octets[1])
            }
        }
        std::net::IpAddr::V6(ip) => ip.is_loopback() || ip.is_unique_local(),
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Login {
    pub host: String,
    pub nonce: String,
    pub actor: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SignIn {
    pub card: Card,
    pub proof: Signed<Login>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantData {
    pub agent: String,
    pub operator: String,
    pub host: String,
    pub request: Uuid,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub agent: Card,
    pub operator: Card,
    pub proof: Signed<GrantData>,
}

impl Grant {
    pub fn issue(agent: &Identity, operator: Card, terms: &Terms) -> Result<Self> {
        ensure!(terms.to == agent.id(), "İstek bu ajana ait değil");
        let proof = agent.sign(
            "operator",
            GrantData {
                agent: agent.id().into(),
                operator: operator.data.key.clone(),
                host: terms.host.clone(),
                request: terms.id,
            },
        )?;
        let grant = Self {
            agent: agent.card.clone(),
            operator,
            proof,
        };
        grant.check()?;
        Ok(grant)
    }

    pub fn check(&self) -> Result<()> {
        self.agent.check()?;
        self.operator.check()?;
        ensure!(
            self.agent.data.role == Role::Agent
                && self.operator.data.role == Role::Human
                && self.proof.data.agent == self.agent.data.key
                && self.proof.data.operator == self.operator.data.key,
            "Geçersiz operatör yetkisi"
        );
        verify(&self.agent.data.key, "operator", &self.proof)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Terms {
    pub id: Uuid,
    pub host: String,
    pub from: String,
    pub to: String,
    pub room: Option<Uuid>,
    pub expires: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionData {
    pub terms: Terms,
    pub accepted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub proof: Signed<DecisionData>,
    pub grant: Option<Grant>,
}

impl Decision {
    pub fn issue(
        signer: &Identity,
        terms: Terms,
        accepted: bool,
        grant: Option<Grant>,
    ) -> Result<Self> {
        Ok(Self {
            proof: signer.sign("decision", DecisionData { terms, accepted })?,
            grant,
        })
    }

    pub fn check(&self, signer: &str, terms: &Terms) -> Result<()> {
        ensure!(
            &self.proof.data.terms == terms,
            "Onay farklı bir isteğe ait"
        );
        if let Some(grant) = &self.grant {
            grant.check()?;
            ensure!(
                grant.agent.data.key == terms.to
                    && grant.operator.data.key == signer
                    && grant.proof.data.host == terms.host
                    && grant.proof.data.request == terms.id,
                "Operatör yetkisi uyuşmuyor"
            );
        } else {
            ensure!(terms.to == signer, "İstek bu hesaba ait değil");
        }
        verify(signer, "decision", &self.proof)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Friendship {
    pub from: Card,
    pub to: Card,
    pub terms: Signed<Terms>,
    pub accepted: Decision,
}

impl Friendship {
    pub fn check(&self, first: &str, second: &str) -> Result<()> {
        let terms = &self.terms.data;
        self.from.check()?;
        self.to.check()?;
        ensure!(
            terms.room.is_none()
                && terms.from == self.from.data.key
                && terms.to == self.to.data.key
                && ((terms.from == first && terms.to == second)
                    || (terms.from == second && terms.to == first)),
            "Arkadaşlık kanıtı bu hesaplara ait değil"
        );
        verify(&terms.from, "offer", &self.terms)?;
        let signer = self
            .accepted
            .grant
            .as_ref()
            .map_or(terms.to.as_str(), |g| g.operator.data.key.as_str());
        self.accepted.check(signer, terms)?;
        ensure!(
            self.accepted.proof.data.accepted,
            "Arkadaşlık kabul edilmemiş"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ticket {
    pub host: HostAddress,
    pub id: Uuid,
    pub code: String,
    pub grant: Option<Grant>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContactRoute {
    pub card: Card,
    pub host: HostAddress,
    pub expires: i64,
}

impl ContactRoute {
    pub fn validate(route: &Signed<Self>) -> Result<()> {
        let value = &route.data;
        value.card.check()?;
        value.host.check()?;
        ensure!(value.expires > now(), "Kişi bağlantısının süresi doldu");
        verify(&value.card.data.key, "contact-route", route)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    Accepted,
    Rejected,
    Expired,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub terms: Signed<Terms>,
    pub from: Card,
    pub to: Card,
    pub room_name: Option<String>,
    pub status: Status,
    pub decision: Option<Decision>,
}

impl Request {
    pub fn friendship(&self) -> Option<Friendship> {
        if self.terms.data.room.is_some() || self.status != Status::Accepted {
            return None;
        }
        Some(Friendship {
            from: self.from.clone(),
            to: self.to.clone(),
            terms: self.terms.clone(),
            accepted: self.decision.clone()?,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Room {
    pub id: Uuid,
    pub name: String,
    pub owner: String,
    pub group: bool,
    pub active: bool,
    pub closed: bool,
    pub members: Vec<Card>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Quote {
    pub id: i64,
    pub author: Card,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub seq: i64,
    pub room: Uuid,
    pub client_id: Uuid,
    pub author: Card,
    pub text: String,
    pub sent_at: i64,
    pub reply: Option<Quote>,
    pub root: Option<i64>,
    pub invitation: Option<Ticket>,
    #[serde(default)]
    pub withdrawn: bool,
    #[serde(default)]
    pub undo_until_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<MessageMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<AttachmentInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Draft {
    pub id: Uuid,
    pub room: Uuid,
    pub text: String,
    pub reply: Option<i64>,
    pub invitation: Option<Ticket>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<MessageMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<Attachment>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageMode {
    Plain,
    Goal,
    Loop { interval_seconds: u64 },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sync {
    pub cursor: i64,
    pub rooms: Vec<Room>,
    pub requests: Vec<Request>,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub withdrawn_messages: Vec<i64>,
    #[serde(default)]
    pub supports_message_modes: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub receipts: Vec<Receipt>,
    #[serde(default)]
    pub supports_attachments: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub avatar_index: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub introductions: Vec<Ticket>,
    #[serde(default)]
    pub max_file_bytes: usize,
    #[serde(default)]
    pub agent_binary_files: bool,
    #[serde(default)]
    pub supports_task_activity: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_activity: Vec<TaskActivity>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskPhase {
    Pending,
    Running,
    WaitingApproval,
    WaitingAnswer,
    Stopping,
    Complete,
    Rejected,
    Cancelled,
    Failed,
}

impl TaskPhase {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Complete | Self::Rejected | Self::Cancelled | Self::Failed
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskActivity {
    pub room: Uuid,
    pub source: i64,
    pub phase: TaskPhase,
    pub plan: Option<[u16; 2]>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Action {
    PublishTaskActivity {
        activity: TaskActivity,
    },
    Sync {
        after: i64,
        grants: Vec<Grant>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        read: BTreeMap<Uuid, i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        avatar: Option<AvatarUpdate>,
    },
    History {
        room: Uuid,
        before: i64,
    },
    Group {
        id: Uuid,
        name: String,
    },
    Offer {
        terms: Signed<Terms>,
        target: Card,
        friendship: Option<Box<Friendship>>,
        code_hash: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        authority: Option<HostAddress>,
    },
    Redeem {
        id: Uuid,
        code: String,
        grant: Option<Grant>,
    },
    Decide {
        decision: Decision,
    },
    Send {
        draft: Draft,
    },
    WithdrawMessage {
        room: Uuid,
        seq: i64,
    },
    CanProcessMessage {
        room: Uuid,
        seq: i64,
    },
    SendBinary {
        draft: Draft,
        name: String,
        bytes: usize,
        hash: String,
    },
    Download {
        room: Uuid,
        seq: i64,
    },
    Avatar {
        actor: String,
        digest: String,
    },
    Leave {
        room: Uuid,
    },
    Remove {
        room: Uuid,
        member: String,
    },
    Close {
        room: Uuid,
    },
    EndFriendship {
        room: Uuid,
    },
    DeliverIntroduction {
        route: Box<Signed<ContactRoute>>,
        ticket: Ticket,
    },
    PublishProfile {
        publication: Box<ProfilePublication>,
    },
    LoadProfile {
        actor: String,
    },
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
