use std::{
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result, ensure};
use directories::UserDirs;
use serde_json::{Value, json};

use super::{TaskTools, check_filename, tool_text};

const TEXT_LIMIT: usize = 32 * 1024;

pub(super) fn definition() -> Value {
    json!({
        "type":"function",
        "name":"chatoid_desktop_text",
        "description":"Create one new UTF-8 .txt file in the Linux user's configured Desktop directory after local approval. Pass only the filename and literal text. If the user requested a desktop screenshot first, wait for chatoid_desktop_image to confirm delivery to chat.",
        "inputSchema":{"type":"object","properties":{"filename":{"type":"string"},"text":{"type":"string"}},"required":["filename","text"],"additionalProperties":false}
    })
}

pub(super) fn call(tools: &TaskTools, params: &Value, cancelled: &AtomicBool) -> Result<Value> {
    create_desktop_text(tools, params, cancelled, desktop_folder)
}

fn create_desktop_text(
    tools: &TaskTools,
    params: &Value,
    cancelled: &AtomicBool,
    folder: impl FnOnce() -> Result<PathBuf>,
) -> Result<Value> {
    ensure!(
        tools.desktop_request.text_file,
        "Bu istek masaüstüne metin yazmaya izin vermiyor"
    );
    ensure!(
        !tools.desktop_request.image_before_text || tools.desktop_delivered(),
        "Önce masaüstü görüntüsünü sohbet eki olarak gönder"
    );
    ensure!(!cancelled.load(Ordering::Acquire), "Görev durduruldu");

    let args = params["arguments"]
        .as_object()
        .context("Masaüstü dosyası parametreleri geçersiz")?;
    let filename = args
        .get("filename")
        .and_then(Value::as_str)
        .context("Dosya adı eksik")?;
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .context("Dosya metni eksik")?;
    ensure!(args.len() == 2, "Masaüstü dosyası parametreleri geçersiz");
    check_filename(filename)?;
    ensure!(
        filename == filename.trim()
            && filename
                .rsplit_once('.')
                .is_some_and(|(stem, ext)| !stem.is_empty() && ext.eq_ignore_ascii_case("txt")),
        "Tek bir geçerli .txt dosya adı gerekli"
    );
    ensure!(
        !text.trim().is_empty() && text.len() <= TEXT_LIMIT && !text.contains('\0'),
        "Masaüstü metni geçersiz veya 32 KiB sınırını aşıyor"
    );

    let mut written = tools.desktop_file.lock().expect("desktop file lock");
    ensure!(
        written.is_none(),
        "Bu görevde masaüstüne bir metin dosyası zaten yazıldı"
    );
    let path = folder()?.join(filename);
    tools.approve(
        "Masaüstü metin dosyası".into(),
        format!("Masaüstünde yeni metin dosyası oluştur\nDosya: {path:?}\nİçerik: {text:?}"),
        false,
        cancelled,
    )?;
    write_text(&path, text, cancelled)?;
    *written = Some(filename.to_owned());
    Ok(tool_text(
        true,
        &format!("Masaüstünde {filename} oluşturuldu ve UTF-8 içeriği doğrulandı."),
    ))
}

fn desktop_folder() -> Result<PathBuf> {
    let directories = UserDirs::new().context("Kullanıcı klasörleri bulunamadı")?;
    let folder = directories
        .desktop_dir()
        .context("XDG masaüstü klasörü yapılandırılmamış")?
        .canonicalize()
        .context("XDG masaüstü klasörü açılamadı")?;
    ensure!(
        folder.is_dir() && folder != directories.home_dir().canonicalize()?,
        "XDG masaüstü klasörü devre dışı veya geçersiz"
    );
    Ok(folder)
}

fn write_text(path: &Path, text: &str, cancelled: &AtomicBool) -> Result<()> {
    let folder = path.parent().context("Masaüstü klasörü bulunamadı")?;
    let mut output =
        tempfile::NamedTempFile::new_in(folder).context("Masaüstü klasörüne yazılamadı")?;
    output.write_all(text.as_bytes())?;
    output.as_file().sync_all()?;
    output.rewind()?;
    let mut verified = Vec::new();
    output
        .as_file()
        .take(TEXT_LIMIT as u64 + 1)
        .read_to_end(&mut verified)?;
    ensure!(
        verified == text.as_bytes(),
        "Masaüstü dosyasının içeriği doğrulanamadı"
    );
    ensure!(!cancelled.load(Ordering::Acquire), "Görev durduruldu");
    output
        .persist_noclobber(path)
        .map_err(|error| error.error)
        .context("Masaüstü dosyası oluşturulamadı. Mevcut dosyaların üzerine yazılmaz.")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs, thread,
        time::{Duration, Instant},
    };

    use super::*;
    use crate::agent::task_tools::{DesktopRequest, definitions};

    #[test]
    fn desktop_text_requires_explicit_request_and_host_delivery() -> Result<()> {
        let mut tools = TaskTools::new(PathBuf::new(), 0, None);
        let cancelled = AtomicBool::new(false);
        let params = json!({"arguments": {"filename": "note.txt", "text": "Merhaba"}});
        let run = |tools: &TaskTools| {
            create_desktop_text(tools, &params, &cancelled, || {
                anyhow::bail!("should not resolve destination")
            })
        };
        assert!(
            !definitions(Some(&tools))
                .iter()
                .any(|tool| tool["name"] == "chatoid_desktop_text")
        );
        assert!(
            run(&tools)
                .unwrap_err()
                .to_string()
                .contains("izin vermiyor")
        );

        tools.configure_desktop(
            DesktopRequest {
                text_file: true,
                image_before_text: true,
            },
            false,
        );
        assert!(
            definitions(Some(&tools))
                .iter()
                .any(|tool| tool["name"] == "chatoid_desktop_text")
        );
        assert!(run(&tools).unwrap_err().to_string().contains("Önce"));
        assert!(tools.pending().is_none());
        Ok(())
    }

    #[test]
    fn desktop_text_needs_local_approval_and_can_write_only_one_file() -> Result<()> {
        let desktop = tempfile::tempdir()?;
        let mut tools = TaskTools::new(PathBuf::new(), 0, None);
        tools.configure_desktop(
            DesktopRequest {
                text_file: true,
                image_before_text: true,
            },
            true,
        );
        let cancelled = AtomicBool::new(false);
        for (filename, accepted) in [("denied.txt", false), ("note.txt", true)] {
            let params = json!({"arguments": {"filename": filename, "text": "Merhaba dünya\n"}});
            let result = thread::scope(|scope| -> Result<_> {
                let worker = scope.spawn(|| {
                    create_desktop_text(&tools, &params, &cancelled, || {
                        Ok(desktop.path().to_owned())
                    })
                });
                let deadline = Instant::now() + Duration::from_secs(2);
                let approval = loop {
                    if let Some(approval) = tools.pending() {
                        break approval;
                    }
                    if Instant::now() >= deadline {
                        cancelled.store(true, Ordering::Release);
                        anyhow::bail!("local approval did not arrive");
                    }
                    thread::sleep(Duration::from_millis(5));
                };
                let untouched = !desktop.path().join(filename).exists();
                tools.decide(approval.id, accepted)?;
                assert!(
                    untouched
                        && approval.action.contains(filename)
                        && approval.action.contains("Merhaba dünya")
                );
                Ok(worker.join().expect("desktop writer thread"))
            })?;
            assert_eq!(result.is_ok(), accepted);
            assert_eq!(desktop.path().join(filename).exists(), accepted);
        }
        assert_eq!(
            fs::read_to_string(desktop.path().join("note.txt"))?,
            "Merhaba dünya\n"
        );
        assert!(tools.desktop_file_written());
        let params = json!({"arguments": {"filename": "another.txt", "text": "Second"}});
        assert!(
            create_desktop_text(&tools, &params, &cancelled, || Ok(desktop
                .path()
                .to_owned()))
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn desktop_text_resolves_xdg_desktop_directory() -> Result<()> {
        const EXPECTED: &str = "CHATOID_TEST_EXPECTED_XDG_DESKTOP";
        if let Some(expected) = std::env::var_os(EXPECTED) {
            if expected.is_empty() {
                assert!(desktop_folder().is_err());
            } else {
                assert_eq!(desktop_folder()?, PathBuf::from(expected));
            }
            return Ok(());
        }

        let root = tempfile::tempdir()?;
        let config = root.path().join("config");
        let desktop = root.path().join("Özel Masaüstü");
        fs::create_dir(&config)?;
        fs::create_dir(&desktop)?;
        let (_, module) = module_path!()
            .split_once("::")
            .context("test module name")?;
        let test = format!("{module}::desktop_text_resolves_xdg_desktop_directory");
        for (setting, expected) in [
            (
                r#"XDG_DESKTOP_DIR="$HOME/Özel Masaüstü""#,
                desktop.as_os_str(),
            ),
            (r#"XDG_DESKTOP_DIR="$HOME/""#, std::ffi::OsStr::new("")),
        ] {
            fs::write(config.join("user-dirs.dirs"), format!("{setting}\n"))?;
            let output = std::process::Command::new(std::env::current_exe()?)
                .args(["--exact", &test, "--nocapture"])
                .env("HOME", root.path())
                .env("XDG_CONFIG_HOME", &config)
                .env(EXPECTED, expected)
                .output()?;
            assert!(
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        Ok(())
    }

    #[test]
    fn desktop_text_preserves_existing_file_and_symlink() -> Result<()> {
        let desktop = tempfile::tempdir()?;
        let existing = desktop.path().join("existing.txt");
        let symlink = desktop.path().join("symlink.txt");
        fs::write(&existing, "initial")?;
        std::os::unix::fs::symlink(&existing, &symlink)?;
        let cancelled = AtomicBool::new(false);
        assert!(write_text(&existing, "new", &cancelled).is_err());
        assert!(write_text(&symlink, "new", &cancelled).is_err());
        assert_eq!(fs::read_to_string(existing)?, "initial");
        assert!(symlink.is_symlink());
        Ok(())
    }
}
