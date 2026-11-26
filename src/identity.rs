use std::{fs, io::Write, path::Path};

use anyhow::{Context, Result, anyhow, ensure};
use argon2::Argon2;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use zeroize::Zeroizing;

use crate::local_file::{open_regular, read_bounded};

const MAX_IDENTITY_FILE_BYTES: usize = 8192;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Human,
    Agent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub key: String,
    pub role: Role,
    pub handle: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signed<T> {
    pub data: T,
    pub signature: String,
}

pub type Card = Signed<Profile>;

#[derive(Clone)]
pub struct Identity {
    key: SigningKey,
    pub card: Card,
    agent_profile: Option<Signed<AgentProfile>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfile {
    pub model: String,
    pub duty: String,
    pub is_sub_agent: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_agent: Option<String>,
}

impl AgentProfile {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.model.trim().is_empty()
                && self.model.chars().count() <= 120
                && !self.model.chars().any(char::is_control),
            "Model adı 1–120 karakter olmalı"
        );
        ensure!(
            !self.duty.trim().is_empty()
                && self.duty.chars().count() <= 800
                && !self
                    .duty
                    .chars()
                    .any(|c| c.is_control() && c != '\n' && c != '\t'),
            "Görev 1–800 karakter olmalı"
        );
        match (&self.parent_agent, self.is_sub_agent) {
            (Some(parent), true) => ensure!(
                !parent.trim().is_empty()
                    && parent.chars().count() <= 80
                    && !parent.chars().any(char::is_control),
                "Üst ajan adı 1–80 karakter olmalı"
            ),
            (None, false) => {}
            _ => anyhow::bail!("Alt ajan için üst ajan adı gerekli"),
        }
        Ok(())
    }
}

pub fn encode(value: impl AsRef<[u8]>) -> String {
    URL_SAFE_NO_PAD.encode(value)
}

pub fn decode<const N: usize>(encoded: &str) -> Result<[u8; N]> {
    URL_SAFE_NO_PAD
        .decode(encoded)?
        .try_into()
        .map_err(|_| anyhow!("Geçersiz veri boyutu"))
}

pub fn random<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("{e}"))?;
    Ok(bytes)
}

pub fn pack<T: Serialize>(kind: &str, value: &T) -> Result<String> {
    Ok(format!(
        "chatoid:{kind}:{}",
        encode(serde_json::to_vec(value)?)
    ))
}

pub fn unpack<T: DeserializeOwned>(kind: &str, text: &str) -> Result<T> {
    let text = text.trim();
    ensure!(text.len() <= 32_768, "Bağlantı kodu çok uzun");
    let data = text
        .strip_prefix(&format!("chatoid:{kind}:"))
        .context("Geçersiz ChatOid kodu")?;
    Ok(serde_json::from_slice(&URL_SAFE_NO_PAD.decode(data)?)?)
}

fn signed_bytes<T: Serialize>(domain: &str, data: &T) -> Result<Vec<u8>> {
    let mut bytes = format!("chatoid/0.1/{domain}\0").into_bytes();
    serde_json::to_writer(&mut bytes, data)?;
    Ok(bytes)
}

pub fn verify<T: Serialize>(key: &str, domain: &str, signed: &Signed<T>) -> Result<()> {
    let key = VerifyingKey::from_bytes(&decode(key)?)?;
    key.verify_strict(
        &signed_bytes(domain, &signed.data)?,
        &Signature::from_bytes(&decode(&signed.signature)?),
    )
    .context("İmza doğrulanamadı")
}

impl Profile {
    fn validate(&self) -> Result<()> {
        validate_handle(&self.handle)?;
        validate_name(&self.name)
    }
}

pub(crate) const HANDLE_RULE: &str = "3–24 karakter. Latin harfi, rakam veya alt çizgi.";
pub(crate) const NAME_RULE: &str = "1–48 karakter. Tek satır.";
pub(crate) const PASSWORD_MIN: usize = 6;
const PASSWORD_MAX_BYTES: usize = 1024;

pub(crate) fn password_rule() -> String {
    format!("En az {PASSWORD_MIN} karakter. En fazla {PASSWORD_MAX_BYTES} bayt.")
}

pub(crate) fn validate_handle(handle: &str) -> Result<()> {
    ensure!(
        (3..=24).contains(&handle.len())
            && handle
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
        "{HANDLE_RULE}"
    );
    Ok(())
}

pub(crate) fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.trim().is_empty()
            && name.chars().count() <= 48
            && !name.chars().any(char::is_control),
        "{NAME_RULE}"
    );
    Ok(())
}

pub(crate) fn validate_password(password: &str) -> Result<()> {
    ensure!(
        password.chars().count() >= PASSWORD_MIN && password.len() <= PASSWORD_MAX_BYTES,
        "{}",
        password_rule()
    );
    Ok(())
}

impl Card {
    pub fn check(&self) -> Result<()> {
        self.data.validate()?;
        verify(&self.data.key, "card", self)
    }
}

#[derive(Serialize, Deserialize)]
struct KeyFile {
    card: Card,
    secret: String,
    protection: Option<Protection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_profile: Option<Signed<AgentProfile>>,
}

#[derive(Serialize, Deserialize)]
struct Protection {
    salt: String,
    nonce: String,
}

fn password_key(password: &str, salt: &[u8; 16]) -> Result<Zeroizing<[u8; 32]>> {
    let mut key = Zeroizing::new([0; 32]);
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt, key.as_mut())
        .map_err(|e| anyhow!("{e}"))?;
    Ok(key)
}

pub fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path).map_err(|error| error.error)?;
    Ok(())
}

impl Identity {
    pub fn generate(role: Role, handle: &str, name: &str) -> Result<Self> {
        let seed = Zeroizing::new(random()?);
        let key = SigningKey::from_bytes(&seed);
        let data = Profile {
            key: encode(key.verifying_key().as_bytes()),
            role,
            handle: handle.trim().to_ascii_lowercase(),
            name: name.trim().into(),
        };
        data.validate()?;
        let card = sign(&key, "card", data)?;
        Ok(Self {
            key,
            card,
            agent_profile: None,
        })
    }

    pub fn generate_agent(handle: &str, name: &str, profile: AgentProfile) -> Result<Self> {
        profile.validate()?;
        let mut identity = Self::generate(Role::Agent, handle, name)?;
        identity.agent_profile = Some(identity.sign("agent-profile", profile)?);
        Ok(identity)
    }

    pub fn agent_profile(&self) -> Option<&AgentProfile> {
        self.agent_profile.as_ref().map(|profile| &profile.data)
    }

    pub(crate) fn session_secret(&self) -> Result<Zeroizing<[u8; 32]>> {
        ensure!(
            self.card.data.role == Role::Human,
            "Yalnızca insan oturumu hatırlanabilir"
        );
        Ok(Zeroizing::new(self.key.to_bytes()))
    }

    pub(crate) fn from_session(path: &Path, seed: &[u8; 32]) -> Result<Self> {
        let bytes = read_bounded(&open_regular(path, true)?, MAX_IDENTITY_FILE_BYTES)?;
        #[derive(Deserialize)]
        struct StoredCard {
            card: Card,
        }
        let StoredCard { card } = serde_json::from_slice(&bytes)?;
        card.check()?;
        let key = SigningKey::from_bytes(seed);
        ensure!(
            card.data.role == Role::Human
                && encode(key.verifying_key().as_bytes()) == card.data.key,
            "Hatırlanan oturum bu hesapla uyuşmuyor"
        );
        Ok(Self {
            key,
            card,
            agent_profile: None,
        })
    }

    pub fn id(&self) -> &str {
        &self.card.data.key
    }

    pub fn sign<T: Serialize>(&self, domain: &str, data: T) -> Result<Signed<T>> {
        sign(&self.key, domain, data)
    }

    pub fn save(&self, path: &Path, password: Option<&str>) -> Result<()> {
        ensure!(
            password.is_some() == (self.card.data.role == Role::Human),
            "Yanlış hesap türü"
        );
        let seed = Zeroizing::new(self.key.to_bytes());
        let (secret, protection) = match password {
            Some(password) => {
                validate_password(password)?;
                let salt = random()?;
                let nonce = random::<24>()?;
                let key = password_key(password, &salt)?;
                let cipher = XChaCha20Poly1305::new((&*key).into());
                let sealed = cipher
                    .encrypt(
                        XNonce::from_slice(&nonce),
                        Payload {
                            msg: seed.as_ref(),
                            aad: self.id().as_bytes(),
                        },
                    )
                    .map_err(|_| anyhow!("Hesap şifrelenemedi"))?;
                (
                    encode(sealed),
                    Some(Protection {
                        salt: encode(salt),
                        nonce: encode(nonce),
                    }),
                )
            }
            None => (encode(seed.as_slice()), None),
        };
        let record = KeyFile {
            card: self.card.clone(),
            secret,
            protection,
            agent_profile: self.agent_profile.clone(),
        };
        let bytes = if record.agent_profile.is_some() {
            serde_json::to_vec_pretty(&record)?
        } else {
            serde_json::to_vec(&record)?
        };
        write_new(path, &Zeroizing::new(bytes))
    }

    pub fn load(path: &Path, role: Role, password: Option<&str>) -> Result<Self> {
        let bytes = read_bounded(&open_regular(path, true)?, MAX_IDENTITY_FILE_BYTES)?;
        let record: KeyFile = serde_json::from_slice(&bytes)?;
        record.card.check()?;
        ensure!(
            record.card.data.role == role,
            "Bu kimlik farklı bir istemciye ait"
        );
        if let Some(profile) = &record.agent_profile {
            ensure!(
                role == Role::Agent,
                "Ajan bilgileri insan hesabına ait olamaz"
            );
            profile.data.validate()?;
            verify(&record.card.data.key, "agent-profile", profile)?;
        }
        let secret = Zeroizing::new(record.secret);
        let seed = match (role, password, record.protection) {
            (Role::Human, Some(password), Some(protection)) => {
                ensure!(password.len() <= PASSWORD_MAX_BYTES, "Geçersiz parola");
                let key = password_key(password, &decode(&protection.salt)?)?;
                let cipher = XChaCha20Poly1305::new((&*key).into());
                let nonce = decode::<24>(&protection.nonce)?;
                let decoded = Zeroizing::new(
                    cipher
                        .decrypt(
                            XNonce::from_slice(&nonce),
                            Payload {
                                msg: &URL_SAFE_NO_PAD.decode(secret.as_bytes())?,
                                aad: record.card.data.key.as_bytes(),
                            },
                        )
                        .map_err(|_| anyhow!("Kullanıcı adı veya parola yanlış"))?,
                );
                Zeroizing::new(<[u8; 32]>::try_from(decoded.as_slice())?)
            }
            (Role::Agent, None, None) => Zeroizing::new(decode(&secret)?),
            _ => return Err(anyhow!("Yanlış hesap türü")),
        };
        let key = SigningKey::from_bytes(&seed);
        ensure!(
            encode(key.verifying_key().as_bytes()) == record.card.data.key,
            "Kimlik anahtarı uyuşmuyor"
        );
        Ok(Self {
            key,
            card: record.card,
            agent_profile: record.agent_profile,
        })
    }
}

fn sign<T: Serialize>(key: &SigningKey, domain: &str, data: T) -> Result<Signed<T>> {
    Ok(Signed {
        signature: encode(key.sign(&signed_bytes(domain, &data)?).to_bytes()),
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_accepts_six_characters_and_rejects_five() -> Result<()> {
        assert!(validate_password("aşbçd").is_err());
        validate_password("aşbçd6")?;
        let dir = tempfile::tempdir()?;
        let human = Identity::generate(Role::Human, "deniz", "Deniz")?;
        let path = dir.path().join("deniz.identity");
        human.save(&path, Some("aşbçd6"))?;
        assert_eq!(
            Identity::load(&path, Role::Human, Some("aşbçd6"))?.id(),
            human.id()
        );
        Ok(())
    }

    #[test]
    fn new_agent_key_preserves_profile_and_imports_old_keys() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut profile = AgentProfile {
            model: "Qwen 2.7 27B".into(),
            duty: "Windows kurulumunu doğrula".into(),
            is_sub_agent: true,
            parent_agent: None,
        };
        assert!(Identity::generate_agent("poyraz", "Poyraz", profile.clone()).is_err());
        profile.parent_agent = Some("Kıvılcım".into());
        let agent = Identity::generate_agent("poyraz", "Poyraz", profile.clone())?;
        let key_path = dir.path().join("poyraz.key");
        agent.save(&key_path, None)?;
        assert_eq!(
            Identity::load(&key_path, Role::Agent, None)?.agent_profile(),
            Some(&profile)
        );
        let old = dir.path().join("old.key");
        Identity::generate(Role::Agent, "old_agent", "Eski ajan")?.save(&old, None)?;
        assert_eq!(
            Identity::load(&old, Role::Agent, None)?.agent_profile(),
            None
        );
        Ok(())
    }
}
