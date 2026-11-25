use std::{
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use eframe::egui::{self, Color32, Id, RichText, Vec2};

const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u32 = 16_384;
const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TEXTURE_SIDE: u32 = 4096;
const LOAD_TIMEOUT: Duration = Duration::from_secs(45);

fn cache_scope() -> &'static Mutex<String> {
    static ID: OnceLock<Mutex<String>> = OnceLock::new();
    ID.get_or_init(|| Mutex::new(uuid::Uuid::new_v4().to_string()))
}
fn process_cache_id() -> String {
    cache_scope().lock().expect("cache scope").clone()
}
pub fn reset_cache_scope() {
    *cache_scope().lock().expect("cache scope") = uuid::Uuid::new_v4().to_string();
}

/// Returns a per-process cache target so a previous account's downloaded file
/// can never be mistaken for the current account's attachment.
pub fn cache_path(root: &Path, host: &str, seq: i64) -> PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    host.hash(&mut hasher);
    root.join("attachment-viewer")
        .join(process_cache_id())
        .join(format!("{:016x}", hasher.finish()))
        .join(format!("{seq}.image"))
}

#[derive(Default)]
pub struct Viewer {
    image_name: String,
    path: Option<PathBuf>,
    opened_at: Option<Instant>,
    decoded: Option<egui::TextureHandle>,
    dimensions: Option<[u32; 2]>,
    error: Option<String>,
    zoom: f32,
    open: bool,
}

impl Viewer {
    pub fn open(&mut self, name: String, path: PathBuf) {
        self.image_name = name;
        self.path = Some(path);
        self.opened_at = Some(Instant::now());
        self.decoded = None;
        self.dimensions = None;
        self.error = None;
        self.zoom = 1.0;
        self.open = true;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn close(&mut self) {
        self.open = false;
        self.decoded = None;
        self.path = None;
        self.opened_at = None;
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        if !self.open {
            return;
        }
        if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            self.close();
            return;
        }
        self.load_if_ready(ctx);

        let mut open = self.open;
        egui::Modal::new(Id::new("attachment-image-viewer")).show(ctx, |ui| {
            let area = ctx.content_rect().size() - Vec2::new(40., 50.);
            let target = Vec2::new(area.x.clamp(250., 1100.), area.y.clamp(200., 820.));
            ui.set_width(target.x);
            ui.set_min_height(target.y);
            ui.set_max_height(target.y);
            ui.horizontal(|ui| {
                ui.add(egui::Label::new(RichText::new(&self.image_name).strong()).truncate());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if super::visual::composer_button(
                        ui,
                        super::visual::Outline::Cross,
                        "Kapat (Esc)",
                        false,
                    )
                    .clicked()
                    {
                        open = false;
                    }
                    if self.decoded.is_some() {
                        if ui.button("+").on_hover_text("Yakınlaştır").clicked() {
                            self.zoom = (self.zoom * 1.2).min(8.0);
                        }
                        if ui.button("−").on_hover_text("Uzaklaştır").clicked() {
                            self.zoom = (self.zoom / 1.2).max(0.1);
                        }
                        if ui.button("Sığdır").clicked() {
                            self.zoom = 1.0;
                        }
                    }
                });
            });
            ui.separator();
            let available = Vec2::new(target.x - 24., (target.y - 84.).max(100.));
            if let Some(texture) = &self.decoded {
                let source = texture.size_vec2();
                let fit = (available.x / source.x)
                    .min(available.y / source.y)
                    .min(1.0);
                let size = source * (fit * self.zoom);
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .max_height(available.y)
                    .show(ui, |ui| {
                        ui.centered_and_justified(|ui| {
                            ui.image((texture.id(), size));
                        });
                    });
                if let Some([width, height]) = self.dimensions {
                    ui.label(
                        RichText::new(format!("{width} × {height} · {:.0}%", self.zoom * 100.0))
                            .small()
                            .color(Color32::from_gray(160)),
                    );
                }
            } else if let Some(error) = &self.error {
                ui.vertical_centered(|ui| {
                    ui.add_space(100.0);
                    ui.label(RichText::new("Görsel açılamadı").strong());
                    ui.label(error);
                });
            } else {
                ui.vertical_centered(|ui| {
                    ui.add_space(100.0);
                    ui.spinner();
                    ui.label("Görsel yükleniyor…");
                });
                ctx.request_repaint_after(Duration::from_millis(250));
            }
        });
        self.open = open;
        if !self.open {
            self.close();
        }
    }

    fn load_if_ready(&mut self, ctx: &egui::Context) {
        if self.decoded.is_some() || self.error.is_some() {
            return;
        }
        let Some(path) = self.path.as_ref() else {
            self.error = Some("Dosya yolu bulunamadı.".into());
            return;
        };
        if !path.is_file() {
            if self
                .opened_at
                .is_some_and(|at| at.elapsed() >= LOAD_TIMEOUT)
            {
                self.error = Some("Dosya indirilmedi veya bağlantı zaman aşımına uğradı.".into());
            }
            return;
        }
        match decode_image(path) {
            Ok((image, dimensions)) => {
                let max_side =
                    MAX_TEXTURE_SIDE.min(ctx.input(|input| input.max_texture_side) as u32);
                let image = if image.width() > max_side || image.height() > max_side {
                    image.thumbnail(max_side, max_side)
                } else {
                    image
                }
                .into_rgba8();
                let color = egui::ColorImage::from_rgba_unmultiplied(
                    [image.width() as usize, image.height() as usize],
                    image.as_raw(),
                );
                self.decoded = Some(ctx.load_texture(
                    format!("attachment-viewer-{}", self.image_name),
                    color,
                    egui::TextureOptions::LINEAR,
                ));
                self.dimensions = Some(dimensions);
            }
            Err(error) => self.error = Some(error),
        }
    }
}

fn decode_image(path: &Path) -> Result<(image::DynamicImage, [u32; 2]), String> {
    let metadata = std::fs::metadata(path).map_err(|_| "Dosya okunamadı.".to_owned())?;
    if metadata.len() == 0 || metadata.len() > MAX_FILE_BYTES {
        return Err("Görsel dosyası boyut sınırını aşıyor.".into());
    }
    let mut reader = image::ImageReader::open(path)
        .map_err(|_| "Görsel dosyası okunamadı.".to_owned())?
        .with_guessed_format()
        .map_err(|_| "Görsel biçimi tanınmıyor.".to_owned())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|_| "Dosya geçerli veya desteklenen bir görsel değil.".to_owned())?;
    let dimensions = [image.width(), image.height()];
    Ok((image, dimensions))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_full_image_and_escape_closes_modal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.png");
        image::RgbImage::new(1920, 1080).save(&path).unwrap();
        let ctx = egui::Context::default();
        let mut viewer = Viewer::default();
        viewer.open("photo.png".into(), path);
        let _ = ctx.run(egui::RawInput::default(), |ctx| viewer.show(ctx));
        assert!(viewer.is_open());
        assert_eq!(viewer.dimensions, Some([1920, 1080]));
        assert!(viewer.decoded.is_some());
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ctx| viewer.show(ctx),
        );
        assert!(!viewer.is_open());
        assert!(viewer.decoded.is_none());
    }

    #[test]
    fn decoder_rejects_non_image_files() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"this is not an image").unwrap();
        assert!(decode_image(file.path()).is_err());
    }

    #[test]
    fn cache_path_is_confined_and_host_specific() {
        let root = Path::new("cache");
        let path = cache_path(root, "../../outside", 4);
        assert!(path.starts_with(root));
        assert_ne!(path, cache_path(root, "another-host", 4));
        assert!(path.ends_with("4.image"));
    }
}
