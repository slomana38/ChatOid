use std::{
    fs,
    io::{self, Write},
    path::Path,
};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::identity::{Identity, validate_handle};

const FILE: &str = "remembered.json";

#[derive(Serialize, Deserialize)]
struct Record {
    handle: String,
    protected: Vec<u8>,
}

fn read(root: &Path) -> Result<Option<Record>> {
    let path = root.join(FILE);
    match fs::metadata(&path) {
        Ok(meta) => ensure!(meta.len() <= 24_000, "Geçersiz oturum dosyası"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let record: Record = serde_json::from_slice(&fs::read(path)?)?;
    validate_handle(&record.handle)?;
    Ok(Some(record))
}

pub fn save(root: &Path, identity: &Identity) -> Result<()> {
    let seed = identity.session_secret()?;
    let previous = read(root).ok().flatten();
    let record = Record {
        handle: identity.card.data.handle.clone(),
        protected: system::protect(seed.as_slice())?,
    };
    let result = (|| -> Result<()> {
        fs::create_dir_all(root)?;
        let mut file = tempfile::NamedTempFile::new_in(root)?;
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

pub fn forget(root: &Path) -> Result<()> {
    let saved = read(root).ok().flatten();
    match fs::remove_file(root.join(FILE)) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    if let Some(record) = saved {
        system::discard(&record.protected)?;
    }
    Ok(())
}

pub fn forget_account(root: &Path, handle: &str) -> Result<()> {
    if read(root)?.is_some_and(|saved| saved.handle == handle) {
        forget(root)?;
    }
    Ok(())
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

#[cfg(not(windows))]
mod system {
    use std::{
        io::Write,
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    use anyhow::{Context, Result, bail, ensure};
    use zeroize::Zeroizing;

    use crate::identity::{decode, encode, random};

    fn command(action: &str, slot: &str, input: Option<&str>) -> Result<Zeroizing<Vec<u8>>> {
        let mut command = Command::new("secret-tool");
        command.arg(action);
        if input.is_some() {
            command.arg("--label=ChatOid");
        }
        let mut child = command
            .args(["application", "chatoid", "session", slot])
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("Linux anahtar kasası için secret-tool gerekli")?;
        if let Some(input) = input
            && let Some(mut stdin) = child.stdin.take()
        {
            stdin.write_all(input.as_bytes())?;
        }
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                ensure!(status.success(), "Linux anahtar kasası oturumu açamadı");
                return Ok(Zeroizing::new(child.wait_with_output()?.stdout));
            }
            if started.elapsed() > Duration::from_secs(8) {
                let _ = child.kill();
                let _ = child.wait();
                bail!("Linux anahtar kasası yanıt vermedi");
            }
            thread::sleep(Duration::from_millis(30));
        }
    }

    fn slot(secret: &[u8]) -> Result<&str> {
        let slot = std::str::from_utf8(secret)?;
        decode::<24>(slot)?;
        Ok(slot)
    }

    pub fn protect(seed: &[u8]) -> Result<Vec<u8>> {
        let slot = encode(random::<24>()?);
        let value = Zeroizing::new(encode(seed));
        command("store", &slot, Some(&value))?;
        Ok(slot.into_bytes())
    }
    pub fn unprotect(protected: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        let bytes = command("lookup", slot(protected)?, None)?;
        let seed = Zeroizing::new(decode::<32>(std::str::from_utf8(&bytes)?.trim())?);
        Ok(Zeroizing::new(seed.to_vec()))
    }
    pub fn discard(protected: &[u8]) -> Result<()> {
        command("clear", slot(protected)?, None)?;
        Ok(())
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::identity::Role;

    #[test]
    fn remembered_session_survives_restart_and_uses_the_original_identity() -> Result<()> {
        let root = tempfile::tempdir()?;
        let identity = Identity::generate(Role::Human, "deniz", "Deniz")?;
        let account = root.path().join("accounts/deniz.identity");
        identity.save(&account, Some("123456"))?;
        save(root.path(), &identity)?;
        let encoded = fs::read(root.path().join(FILE))?;
        assert!(!encoded.windows(6).any(|window| window == b"123456"));
        assert_eq!(load(root.path())?.expect("remembered").id(), identity.id());
        forget_account(root.path(), "bobby")?;
        assert!(load(root.path())?.is_some());
        fs::remove_file(&account)?;
        Identity::generate(Role::Human, "deniz", "Yeni Deniz")?.save(&account, Some("abcdef"))?;
        assert!(load(root.path()).is_err());
        forget_account(root.path(), "deniz")?;
        assert!(load(root.path())?.is_none());
        Ok(())
    }
}
