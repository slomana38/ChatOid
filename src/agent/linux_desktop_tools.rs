use std::{
    fs::{self, OpenOptions},
    io::{Cursor, Read},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use futures_util::StreamExt;
use serde_json::{Value, json};
use url::Url;
use zbus::{
    Connection, Proxy,
    zvariant::{OwnedObjectPath, OwnedValue, Value as DbusValue},
};

use super::{
    app_server::{TaskResult, tool_text},
    task_tools::TaskTools,
};

#[path = "linux_desktop_tools/window.rs"]
mod window;
#[path = "linux_desktop_tools/x11.rs"]
mod x11;

pub use x11::{Target, is_native_x11_session, window_support_available, windows};

const MAX_PORTAL_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const PORTAL_TIMEOUT: Duration = Duration::from_secs(90);
const PORTAL_NAME: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SCREENSHOT_INTERFACE: &str = "org.freedesktop.portal.Screenshot";
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";
const SCREEN_TARGET: u32 = 1;

pub fn definitions() -> Vec<Value> {
    let mut tools = vec![json!({
        "type":"function",
        "name":"chatoid_desktop_image",
        "description":"Request one desktop screenshot through the interactive Linux screenshot portal. The system dialog may require an operator choice. Enable share_in_chat only when the user requested that the image be included in ChatOid.",
        "inputSchema":{"type":"object","properties":{"share_in_chat":{"type":"boolean"}},"required":["share_in_chat"],"additionalProperties":false}
    })];
    if window_support_available() {
        tools.extend(window::definitions());
    }
    tools
}

pub fn call(
    tools: &TaskTools,
    params: &Value,
    result: &mut TaskResult,
    cancelled: &AtomicBool,
) -> Result<Value> {
    if params["tool"] != "chatoid_desktop_image" {
        return window::call(tools, params, result, cancelled);
    }
    complete_screenshot(tools, params, result, cancelled, || {
        capture_portal(cancelled)
    })
}

fn complete_screenshot(
    tools: &TaskTools,
    params: &Value,
    result: &mut TaskResult,
    cancelled: &AtomicBool,
    capture: impl FnOnce() -> Result<Vec<u8>>,
) -> Result<Value> {
    ensure!(
        params["tool"] == "chatoid_desktop_image",
        "Bu görev aracı kullanılabilir değil"
    );
    let args = params["arguments"]
        .as_object()
        .context("Görüntü parametreleri geçersiz")?;
    ensure!(args.len() == 1, "Görüntü parametreleri geçersiz");
    let share = args
        .get("share_in_chat")
        .and_then(Value::as_bool)
        .context("Görüntü paylaşım seçimi eksik")?
        || tools.requested_desktop_capture_allowed();
    if tools.desktop_request.image_before_text && tools.desktop_delivered() {
        return Ok(tool_text(
            true,
            "Masaüstü görüntüsü bu görev için sohbete zaten teslim edildi.",
        ));
    }
    ensure!(
        !share || (result.file.is_none() && result.screenshot.is_none()),
        "Görev sonucu için bir ek zaten onaylandı"
    );
    let action = if share {
        "Tam masaüstü görüntüsünü bir kez yakala ve görüntüyü Codex’e ve görev sohbetine gönder."
    } else {
        "Masaüstü görüntüsünü yakala ve yalnızca değerlendirmesi için Codex’e gönder."
    };
    tools.approve_desktop_capture(share, action.into(), cancelled)?;
    ensure!(!cancelled.load(Ordering::Acquire), "Görev durduruldu");
    let image = capture()?;
    ensure!(!cancelled.load(Ordering::Acquire), "Görev durduruldu");
    ensure!(
        !image.is_empty() && image.len() <= crate::protocol::MAX_ATTACHMENT,
        "Masaüstü görüntüsü çok büyük veya boş"
    );
    let url = format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&image)
    );
    if share && tools.desktop_request.image_before_text {
        tools.deliver_desktop(image, cancelled)?;
    } else if share {
        result.screenshot = Some(image);
    }
    Ok(json!({
        "success":true,
        "contentItems":[{"type":"inputImage","imageUrl":url}]
    }))
}

fn capture_portal(cancelled: &AtomicBool) -> Result<Vec<u8>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("Linux ekran görüntüsü portalı başlatılamadı")?;
    runtime.block_on(async {
        ensure!(!cancelled.load(Ordering::Acquire), "Görev durduruldu");
        let connection = Connection::session()
            .await
            .context("XDG oturum veriyoluna bağlanılamadı")?;
        let screenshot_proxy =
            Proxy::new(&connection, PORTAL_NAME, PORTAL_PATH, SCREENSHOT_INTERFACE)
                .await
                .context("XDG ekran görüntüsü portalı kullanılamıyor")?;
        let version: u32 = screenshot_proxy
            .get_property("version")
            .await
            .context("XDG ekran görüntüsü portalı sürümü alınamadı")?;
        let target = if version >= 3 {
            let available_targets: u32 = screenshot_proxy
                .get_property("AvailableTargets")
                .await
                .context("XDG portalın ekran görüntüsü hedefleri alınamadı")?;
            screen_target_for_portal(version, available_targets)?
        } else {
            screen_target_for_portal(version, 0)?
        };
        let sender = connection
            .unique_name()
            .context("XDG oturum veriyolunda ChatOid adı alınamadı")?
            .as_str()
            .trim_start_matches(':')
            .replace('.', "_");
        let token = format!("chatoid_{}", uuid::Uuid::new_v4().simple());
        let expected_path = format!("{PORTAL_PATH}/request/{sender}/{token}");
        let request_proxy = Proxy::new(
            &connection,
            PORTAL_NAME,
            expected_path.as_str(),
            REQUEST_INTERFACE,
        )
        .await
        .context("XDG ekran görüntüsü isteği hazırlanamadı")?;
        let mut response_signals = request_proxy
            .receive_signal("Response")
            .await
            .context("XDG ekran görüntüsü yanıtı dinlenemedi")?;

        let mut options = std::collections::HashMap::<String, OwnedValue>::new();
        options.insert(
            "handle_token".into(),
            OwnedValue::try_from(DbusValue::new(token.clone()))?,
        );
        options.insert(
            "interactive".into(),
            OwnedValue::try_from(DbusValue::new(true))?,
        );
        options.insert("modal".into(), OwnedValue::try_from(DbusValue::new(true))?);
        if let Some(target) = target {
            options.insert(
                "target".into(),
                OwnedValue::try_from(DbusValue::new(target))?,
            );
        }
        let returned_path: OwnedObjectPath = screenshot_proxy
            .call("Screenshot", &("", options))
            .await
            .context("XDG ekran görüntüsü isteği başlatılamadı")?;
        if returned_path.as_str() != expected_path {
            let unexpected_request =
                Proxy::new(&connection, PORTAL_NAME, returned_path, REQUEST_INTERFACE).await;
            if let Ok(unexpected_request) = unexpected_request {
                let _: std::result::Result<(), _> = unexpected_request.call("Close", &()).await;
            }
            bail!("XDG portal farklı bir ekran görüntüsü isteği döndürdü");
        }
        let deadline = Instant::now() + PORTAL_TIMEOUT;
        let signal = loop {
            if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                let _: std::result::Result<(), _> = request_proxy.call("Close", &()).await;
                ensure!(!cancelled.load(Ordering::Acquire), "Görev durduruldu");
                bail!("Linux ekran görüntüsü isteğinin süresi doldu");
            }
            tokio::select! {
                signal = response_signals.next() => {
                    break signal.context("XDG ekran görüntüsü yanıtı alınamadı")?;
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
        };
        let (status, fields): (u32, std::collections::HashMap<String, OwnedValue>) = signal
            .body()
            .deserialize()
            .context("XDG ekran görüntüsü yanıtı geçersiz")?;
        ensure!(status == 0, "XDG ekran görüntüsü isteği iptal edildi");
        let uri_value = fields
            .get("uri")
            .context("XDG portal yanıtında görüntü yolu eksik")?
            .try_clone()?;
        let uri = String::try_from(uri_value).context("XDG portal görüntü yolu geçersiz")?;
        let path = screenshot_path(&uri)?;
        let bytes = read_portal_image(&path)?;
        normalize_image(&bytes)
    })
}

fn screen_target_for_portal(version: u32, available_targets: u32) -> Result<Option<u32>> {
    ensure!(
        version >= 2,
        "XDG portal etkileşimli ekran görüntüsünü desteklemiyor"
    );
    if version >= 3 {
        ensure!(
            available_targets & SCREEN_TARGET != 0,
            "XDG portal ekran görüntüsü için ekran hedefini desteklemiyor"
        );
        Ok(Some(SCREEN_TARGET))
    } else {
        // Screenshot v2 has interactive capture, but its target option was added in v3.
        Ok(None)
    }
}

fn screenshot_path(uri: &str) -> Result<std::path::PathBuf> {
    let uri = Url::parse(uri).context("XDG portal geçersiz görüntü yolu döndürdü")?;
    ensure!(
        uri.scheme() == "file",
        "XDG portal yerel dosya yolu döndürmedi"
    );
    ensure!(
        uri.host_str().is_none_or(|host| host == "localhost"),
        "XDG portal yerel olmayan görüntü yolu döndürdü"
    );
    let path = uri
        .to_file_path()
        .map_err(|_| anyhow::anyhow!("XDG portal görüntü yolu okunamadı"))?;
    ensure!(
        path.is_absolute(),
        "XDG portal geçersiz görüntü yolu döndürdü"
    );
    Ok(path)
}

fn read_portal_image(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).context("Portal görüntüsü açılamadı")?;
    ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "Portal normal bir görüntü dosyası döndürmedi"
    );
    ensure!(
        metadata.len() > 0 && metadata.len() <= MAX_PORTAL_IMAGE_BYTES as u64,
        "Portal görüntüsü çok büyük veya boş"
    );
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).context("Portal görüntüsü açılamadı")?;
    ensure!(
        file.metadata()?.is_file(),
        "Portal normal bir görüntü dosyası döndürmedi"
    );
    let mut bytes = Vec::new();
    file.take(MAX_PORTAL_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_PORTAL_IMAGE_BYTES,
        "Portal görüntüsü çok büyük veya boş"
    );
    Ok(bytes)
}

fn normalize_image(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    ensure!(
        matches!(
            reader.format(),
            Some(image::ImageFormat::Jpeg | image::ImageFormat::Png)
        ),
        "Portal JPEG veya PNG görüntüsü döndürmedi"
    );
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(160 * 1024 * 1024);
    reader.limits(limits);
    let image = reader
        .decode()
        .context("Portal görüntüsü açılamadı")?
        .thumbnail(1280, 960);
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 82).encode_image(&image)?;
    ensure!(
        jpeg.len() <= crate::protocol::MAX_ATTACHMENT,
        "Masaüstü görüntüsü çok büyük"
    );
    Ok(jpeg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use std::{
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::Instant,
    };

    #[test]
    fn linux_advertises_portal_screenshot() {
        let tools = definitions();
        assert!(
            tools
                .iter()
                .any(|tool| tool["name"] == "chatoid_desktop_image")
        );
    }

    #[test]
    fn explicitly_requested_capture_uses_fake_portal_and_attaches_image() -> Result<()> {
        let tools = TaskTools::new(PathBuf::new(), 0, None);
        tools.allow_requested_desktop_capture();
        let mut result = TaskResult::default();
        let cancelled = AtomicBool::new(false);
        let called = std::cell::Cell::new(false);
        let reply = complete_screenshot(
            &tools,
            &json!({"tool":"chatoid_desktop_image","arguments":{"share_in_chat":true}}),
            &mut result,
            &cancelled,
            || {
                called.set(true);
                fake_jpeg()
            },
        );
        assert!(called.get());
        assert!(reply.is_ok());
        let screenshot = result.screenshot.as_deref().context("screenshot missing")?;
        assert!(
            image::ImageReader::new(Cursor::new(screenshot))
                .with_guessed_format()?
                .decode()
                .is_ok()
        );
        assert!(!tools.requested_desktop_capture_allowed());
        Ok(())
    }

    #[test]
    fn ordered_capture_waits_for_chat_acknowledgment() -> Result<()> {
        use super::super::task_tools::DesktopRequest;

        let mut tools = TaskTools::new(PathBuf::new(), 0, None);
        tools.configure_desktop(
            DesktopRequest {
                text_file: true,
                image_before_text: true,
            },
            false,
        );
        let cancelled = AtomicBool::new(false);
        let params = json!({"tool":"chatoid_desktop_image","arguments":{"share_in_chat":true}});
        for acknowledged in [false, true] {
            tools.allow_requested_desktop_capture();
            let result = thread::scope(|scope| -> Result<_> {
                let worker = scope.spawn(|| {
                    let mut outcome = TaskResult::default();
                    let result =
                        complete_screenshot(&tools, &params, &mut outcome, &cancelled, fake_jpeg);
                    assert!(outcome.screenshot.is_none());
                    result
                });
                let deadline = Instant::now() + Duration::from_secs(2);
                let delivery = loop {
                    if let Some(delivery) = tools.take_desktop_delivery() {
                        break delivery;
                    }
                    if Instant::now() >= deadline {
                        cancelled.store(true, Ordering::Release);
                        bail!("screenshot delivery did not arrive");
                    }
                    thread::sleep(Duration::from_millis(5));
                };
                assert!(!tools.desktop_delivered() && !worker.is_finished());
                assert!(image::load_from_memory(&delivery.image).is_ok());
                let response = if acknowledged {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("host unavailable"))
                };
                delivery.reply.send(response)?;
                Ok(worker.join().expect("screenshot worker"))
            })?;
            assert_eq!(result.is_ok(), acknowledged);
            assert_eq!(tools.desktop_delivered(), acknowledged);
        }
        let duplicate = complete_screenshot(
            &tools,
            &params,
            &mut TaskResult::default(),
            &cancelled,
            || bail!("should not repeat an acknowledged capture"),
        )?;
        assert_eq!(duplicate["success"], true);
        Ok(())
    }

    #[test]
    fn unrequested_capture_waits_for_local_approval_before_portal() -> Result<()> {
        let tools = Arc::new(TaskTools::new(PathBuf::new(), 0, None));
        let worker_tools = tools.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let captured = Arc::new(AtomicBool::new(false));
        let worker_captured = captured.clone();
        let worker = thread::spawn(move || {
            let mut result = TaskResult::default();
            complete_screenshot(
                &worker_tools,
                &json!({"tool":"chatoid_desktop_image","arguments":{"share_in_chat":false}}),
                &mut result,
                &worker_cancelled,
                || {
                    worker_captured.store(true, Ordering::Release);
                    fake_jpeg()
                },
            )
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while tools.pending().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let approval = tools.pending().context("capture approval did not arrive")?;
        tools.decide(approval.id, false)?;
        assert!(
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("worker panicked"))?
                .is_err()
        );
        assert!(!captured.load(Ordering::Acquire));
        Ok(())
    }

    #[test]
    fn screenshot_uri_must_be_a_local_file() {
        assert!(screenshot_path("https://example.invalid/image.png").is_err());
        assert!(screenshot_path("file://remote.example/tmp/image.png").is_err());
    }

    #[test]
    fn screenshot_uses_only_targets_supported_by_the_portal_version() {
        assert_eq!(
            screen_target_for_portal(3, SCREEN_TARGET).unwrap(),
            Some(SCREEN_TARGET)
        );
        assert_eq!(screen_target_for_portal(2, 0).unwrap(), None);
        assert!(screen_target_for_portal(1, SCREEN_TARGET).is_err());
        assert!(screen_target_for_portal(3, 0).is_err());
    }

    fn fake_jpeg() -> Result<Vec<u8>> {
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::new(2, 2));
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 80).encode_image(&image)?;
        Ok(bytes)
    }
}
