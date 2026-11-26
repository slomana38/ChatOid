use std::{
    fs,
    io::{self, Write},
    path::Path,
};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{
    identity::{Identity, validate_handle},
    local_file::{open_regular, read_bounded},
};

const FILE: &str = "remembered.json";
const CLEANUP_FILE: &str = "remembered.cleanup";
const MAX_RECORD_BYTES: usize = 24_000;

#[derive(Serialize, Deserialize)]
struct Record {
    handle: String,
    protected: Vec<u8>,
}

fn read(root: &Path) -> Result<Option<Record>> {
    read_record(root, FILE)
}

fn read_record(root: &Path, name: &str) -> Result<Option<Record>> {
    let path = root.join(name);
    let file = match open_regular(&path, false) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let metadata = file.metadata()?;
        // Leave an unsafe inode untouched. Successful password login can replace the record.
        ensure!(
            metadata.mode() & 0o077 == 0 && metadata.nlink() == 1,
            "Hatırlanan oturum dosyası izinleri veya bağlantıları güvenli değil"
        );
    }
    let record: Record = serde_json::from_slice(&read_bounded(&file, MAX_RECORD_BYTES)?)?;
    validate_handle(&record.handle)?;
    Ok(Some(record))
}

pub fn save(root: &Path, identity: &Identity) -> Result<()> {
    let seed = identity.session_secret()?;
    clear_pending(root)?;
    let previous = read(root).ok().flatten();
    let record = Record {
        handle: identity.card.data.handle.clone(),
        protected: system::protect(seed.as_slice())?,
    };
    let result = (|| -> Result<()> {
        fs::create_dir_all(root)?;
        let mut file = tempfile::NamedTempFile::new_in(root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(&serde_json::to_vec(&record)?)?;
        file.as_file().sync_all()?;
        file.persist(root.join(FILE)).map_err(|error| error.error)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = system::discard(&record.protected);
    } else if let Some(previous) = previous {
        let _ = system::discard(&previous.protected);
    }
    result
}

pub fn load(root: &Path) -> Result<Option<Identity>> {
    let Some(record) = read(root)? else {
        return Ok(None);
    };
    let seed = system::unprotect(&record.protected)?;
    let seed: &[u8; 32] = seed.as_slice().try_into()?;
    let identity = Identity::from_session(
        &root
            .join("accounts")
            .join(format!("{}.identity", record.handle)),
        seed,
    )?;
    ensure!(
        identity.card.data.handle == record.handle,
        "Hatırlanan kullanıcı adı uyuşmuyor"
    );
    Ok(Some(identity))
}

fn remove_record(root: &Path, name: &str) -> Result<()> {
    match fs::remove_file(root.join(name)) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

fn clear_pending(root: &Path) -> Result<()> {
    if let Some(record) = read_record(root, CLEANUP_FILE)? {
        system::discard(&record.protected)?;
        remove_record(root, CLEANUP_FILE)?;
    }
    Ok(())
}

fn retire_record(root: &Path) -> Result<()> {
    // Disable automatic sign-in before deleting the corresponding keyring item.
    fs::rename(root.join(FILE), root.join(CLEANUP_FILE))?;
    Ok(())
}

pub fn forget(root: &Path) -> Result<()> {
    clear_pending(root)?;
    if read(root).ok().flatten().is_some() {
        retire_record(root)?;
        clear_pending(root)
    } else {
        remove_record(root, FILE)
    }
}

pub fn forget_account(root: &Path, handle: &str) -> Result<Option<anyhow::Error>> {
    forget_account_with(root, handle, clear_pending)
}

fn forget_account_with(
    root: &Path,
    handle: &str,
    cleanup: impl Fn(&Path) -> Result<()>,
) -> Result<Option<anyhow::Error>> {
    if read(root)?.is_some_and(|saved| saved.handle == handle) {
        cleanup(root)?;
        retire_record(root)?;
        return Ok(cleanup(root).err());
    }
    Ok(match read_record(root, CLEANUP_FILE) {
        Ok(Some(saved)) if saved.handle == handle => cleanup(root).err(),
        Ok(_) => None,
        Err(error) => Some(error),
    })
}

#[cfg(windows)]
mod system {
    use std::{
        io,
        ptr::{null, null_mut},
    };

    use anyhow::{Result, ensure};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        },
    };
    use zeroize::{Zeroize, Zeroizing};

    fn transform(bytes: &[u8], seal: bool) -> Result<Zeroizing<Vec<u8>>> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(bytes.len())?,
            pbData: bytes.as_ptr().cast_mut(),
        };
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: null_mut(),
        };
        // DPAPI binds the blob to the current OS user. No machine-wide flag is used.
        let ok = unsafe {
            if seal {
                CryptProtectData(
                    &input,
                    null(),
                    null(),
                    null(),
                    null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            } else {
                CryptUnprotectData(
                    &input,
                    null_mut(),
                    null(),
                    null(),
                    null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            }
        };
        if ok == 0 {
            return Err(io::Error::last_os_error().into());
        }
        ensure!(!output.pbData.is_null(), "Windows oturum verisi döndürmedi");
        // DPAPI allocated this buffer and transfers ownership to the caller.
        let copy = unsafe {
            let data = std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize);
            let copy = Zeroizing::new(data.to_vec());
            data.zeroize();
            LocalFree(output.pbData.cast());
            copy
        };
        Ok(copy)
    }

    pub fn protect(seed: &[u8]) -> Result<Vec<u8>> {
        Ok(transform(seed, true)?.to_vec())
    }
    pub fn unprotect(protected: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        transform(protected, false)
    }
    pub fn discard(_: &[u8]) -> Result<()> {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod system {
    use std::{collections::HashMap, future::Future, time::Duration};

    use anyhow::{Context, Result};
    use secret_service::{EncryptionType, SecretService};
    use zeroize::Zeroizing;

    use crate::identity::{decode, encode, random};

    fn run<T>(task: impl Future<Output = Result<T>>) -> Result<T> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(8), task)
                    .await
                    .context("Linux anahtar kasası yanıt vermedi")?
            })
    }

    async fn connect() -> Result<SecretService<'static>> {
        SecretService::connect(EncryptionType::Dh)
            .await
            .context("Linux anahtar kasasına (Secret Service) bağlanılamadı")
    }

    fn attributes(slot: &str) -> HashMap<&str, &str> {
        // Match the attributes originally used with libsecret's secret-tool.
        HashMap::from([("application", "chatoid"), ("session", slot)])
    }

    fn slot(secret: &[u8]) -> Result<&str> {
        let slot = std::str::from_utf8(secret)?;
        decode::<24>(slot)?;
        Ok(slot)
    }

    pub fn protect(seed: &[u8]) -> Result<Vec<u8>> {
        let slot = encode(random::<24>()?);
        let value = Zeroizing::new(encode(seed));
        run(async {
            let service = connect().await?;
            let collection = service.get_default_collection().await?;
            if collection.is_locked().await? {
                collection.unlock().await?;
            }
            collection
                .create_item(
                    "ChatOid",
                    attributes(&slot),
                    value.as_bytes(),
                    true,
                    "text/plain",
                )
                .await?;
            Ok(())
        })?;
        Ok(slot.into_bytes())
    }

    pub fn unprotect(protected: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        let slot = slot(protected)?;
        let bytes = run(async {
            let service = connect().await?;
            let items = service.search_items(attributes(slot)).await?;
            let item = if let Some(item) = items.unlocked.first() {
                item
            } else {
                let item = items
                    .locked
                    .first()
                    .context("Hatırlanan oturum anahtar kasasında bulunamadı")?;
                item.unlock().await?;
                item
            };
            Ok(Zeroizing::new(item.get_secret().await?))
        })?;
        let seed = Zeroizing::new(decode::<32>(std::str::from_utf8(&bytes)?.trim())?);
        Ok(Zeroizing::new(seed.to_vec()))
    }

    pub fn discard(protected: &[u8]) -> Result<()> {
        let Ok(slot) = slot(protected) else {
            return Ok(());
        };
        run(async {
            let service = connect().await?;
            let items = service.search_items(attributes(slot)).await?;
            for item in items.unlocked {
                item.delete().await?;
            }
            for item in items.locked {
                item.unlock().await?;
                item.delete().await?;
            }
            Ok(())
        })
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod system {
    use anyhow::{Result, bail};
    use zeroize::Zeroizing;

    pub fn protect(_: &[u8]) -> Result<Vec<u8>> {
        bail!("Bu platformda oturum hatırlama kullanılamıyor")
    }

    pub fn unprotect(_: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        bail!("Bu platformda oturum hatırlama kullanılamıyor")
    }

    pub fn discard(_: &[u8]) -> Result<()> {
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod record_tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn rejects_unsafe_record_without_changing_permissions() -> Result<()> {
        let root = tempfile::tempdir()?;
        let record = root.path().join(FILE);
        let other = root.path().join("other");
        fs::write(&other, br#"{"handle":"deniz","protected":[]}"#)?;
        fs::set_permissions(&other, fs::Permissions::from_mode(0o644))?;

        symlink(&other, &record)?;
        assert!(read(root.path()).is_err());
        fs::remove_file(&record)?;

        fs::hard_link(&other, &record)?;
        assert!(read(root.path()).is_err());
        assert_eq!(fs::metadata(&other)?.permissions().mode() & 0o777, 0o644);
        fs::set_permissions(&other, fs::Permissions::from_mode(0o600))?;
        assert!(read(root.path()).is_err());

        fs::remove_file(other)?;
        assert_eq!(read(root.path())?.unwrap().handle, "deniz");
        Ok(())
    }

    #[test]
    fn logout_can_complete_when_keyring_cleanup_fails() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut metadata = tempfile::NamedTempFile::new_in(root.path())?;
        serde_json::to_writer(
            &mut metadata,
            &Record {
                handle: "deniz".into(),
                protected: vec![1],
            },
        )?;
        metadata.persist(root.path().join(FILE))?;

        fs::create_dir(root.path().join(CLEANUP_FILE))?;
        assert!(forget_account_with(root.path(), "deniz", |_| Ok(())).is_err());
        assert!(root.path().join(FILE).exists());
        fs::remove_dir(root.path().join(CLEANUP_FILE))?;

        let error = forget_account_with(root.path(), "deniz", |path| {
            ensure!(!path.join(CLEANUP_FILE).exists(), "keyring unavailable");
            Ok(())
        })?;
        assert!(error.is_some());
        assert!(load(root.path())?.is_none());
        assert!(root.path().join(CLEANUP_FILE).exists());
        assert!(
            forget_account_with(root.path(), "deniz", |path| remove_record(
                path,
                CLEANUP_FILE
            ))?
            .is_none()
        );
        assert!(!root.path().join(CLEANUP_FILE).exists());
        Ok(())
    }
}

#[cfg(all(test, any(windows, target_os = "linux")))]
mod tests {
    use super::*;
    use crate::identity::Role;

    #[test]
    #[cfg_attr(
        target_os = "linux",
        ignore = "requires an unlocked Secret Service keyring"
    )]
    fn remembered_session_survives_restart_and_uses_the_original_identity() -> Result<()> {
        let root = tempfile::tempdir()?;
        let identity = Identity::generate(Role::Human, "deniz", "Deniz")?;
        let account = root.path().join("accounts/deniz.identity");
        identity.save(&account, Some("123456"))?;
        save(root.path(), &identity)?;
        #[cfg(target_os = "linux")]
        let old_slot = read(root.path())?.unwrap().protected;
        let encoded = fs::read(root.path().join(FILE))?;
        assert!(!encoded.windows(6).any(|window| window == b"123456"));
        assert_eq!(load(root.path())?.expect("remembered").id(), identity.id());
        assert!(forget_account(root.path(), "bobby")?.is_none());
        assert!(load(root.path())?.is_some());
        fs::remove_file(&account)?;
        let replacement = Identity::generate(Role::Human, "deniz", "Yeni Deniz")?;
        replacement.save(&account, Some("abcdef"))?;
        assert!(load(root.path()).is_err());
        assert!(forget_account(root.path(), "deniz")?.is_none());
        assert!(load(root.path())?.is_none());
        assert!(!root.path().join(CLEANUP_FILE).exists());
        #[cfg(target_os = "linux")]
        assert!(system::unprotect(&old_slot).is_err());

        save(root.path(), &replacement)?;
        fs::rename(root.path().join(FILE), root.path().join(CLEANUP_FILE))?;
        assert!(load(root.path())?.is_none());
        assert!(forget_account(root.path(), "deniz")?.is_none());
        assert!(!root.path().join(CLEANUP_FILE).exists());
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires the legacy secret-tool executable and an unlocked Secret Service keyring"]
    fn reads_session_saved_with_legacy_secret_tool() -> Result<()> {
        use std::process::{Command, Stdio};

        use crate::identity::{encode, random};
        use zeroize::Zeroizing;

        let root = tempfile::tempdir()?;
        let identity = Identity::generate(Role::Human, "deniz", "Deniz")?;
        identity.save(&root.path().join("accounts/deniz.identity"), Some("123456"))?;
        let seed = Zeroizing::new(encode(identity.session_secret()?.as_slice()));
        let slot = encode(random::<24>()?);
        let mut tool = Command::new("secret-tool")
            .args([
                "store",
                "--label=ChatOid",
                "application",
                "chatoid",
                "session",
                &slot,
            ])
            .stdin(Stdio::piped())
            .spawn()?;
        tool.stdin.take().unwrap().write_all(seed.as_bytes())?;
        assert!(tool.wait()?.success());

        let mut metadata = tempfile::NamedTempFile::new_in(root.path())?;
        serde_json::to_writer(
            &mut metadata,
            &Record {
                handle: "deniz".into(),
                protected: slot.into_bytes(),
            },
        )?;
        metadata.persist(root.path().join(FILE))?;
        assert_eq!(load(root.path())?.unwrap().id(), identity.id());
        forget(root.path())?;
        assert!(load(root.path())?.is_none());
        Ok(())
    }
}
