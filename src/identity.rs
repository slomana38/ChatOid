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
        ensure!(
            (3..=24).contains(&self.handle.len())
                && self
                    .handle
                    .bytes()
                    .all(|b| { b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' }),
            "Kullanıcı adı 3–24 küçük Latin harfi, rakam veya alt çizgi içermeli"
        );
        ensure!(
            !self.name.trim().is_empty()
                && self.name.chars().count() <= 48
                && !self.name.chars().any(char::is_control),
            "Görünen ad 1–48 karakter olmalı"
        );
        Ok(())
    }
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
        Ok(Self { key, card })
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
                ensure!(
                    password.chars().count() >= 10 && password.len() <= 1024,
                    "Parola en az 10 karakter olmalı"
                );
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
        };
        write_new(path, &Zeroizing::new(serde_json::to_vec(&record)?))
    }

    pub fn load(path: &Path, role: Role, password: Option<&str>) -> Result<Self> {
        ensure!(fs::metadata(path)?.len() <= 8192, "Geçersiz kimlik dosyası");
        let bytes = Zeroizing::new(fs::read(path)?);
        let record: KeyFile = serde_json::from_slice(&bytes)?;
        record.card.check()?;
        ensure!(
            record.card.data.role == role,
            "Bu kimlik farklı bir istemciye ait"
        );
        let secret = Zeroizing::new(record.secret);
        let seed = match (role, password, record.protection) {
            (Role::Human, Some(password), Some(protection)) => {
                ensure!(password.len() <= 1024, "Geçersiz parola");
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
        })
    }
}

fn sign<T: Serialize>(key: &SigningKey, domain: &str, data: T) -> Result<Signed<T>> {
    Ok(Signed {
        signature: encode(key.sign(&signed_bytes(domain, &data)?).to_bytes()),
        data,
    })
}
