use std::{
    net::SocketAddr,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::identity::{Card, Identity, Role, Signed, decode, verify};

pub const VERSION: u8 = 1;
pub const PAGE: usize = 200;
pub const INVITE_LIFETIME: i64 = 7 * 24 * 3600;
pub const MAX_TEXT: usize = 4000;

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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Draft {
    pub id: Uuid,
    pub room: Uuid,
    pub text: String,
    pub reply: Option<i64>,
    pub invitation: Option<Ticket>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sync {
    pub cursor: i64,
    pub rooms: Vec<Room>,
    pub requests: Vec<Request>,
    pub messages: Vec<Message>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Action {
    Sync {
        after: i64,
        grants: Vec<Grant>,
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
}
