use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DraftState {
    pub text: String,
    pub reply: Option<(i64, String, String)>,
    pub attachment: Option<PathBuf>,
}

#[derive(Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub enum Background {
    #[default]
    Orbit,
    Plain,
    Photo,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Wallpaper {
    pub background: Background,
    pub color: [u8; 3],
    pub dim: u8,
    pub photo: Option<PathBuf>,
}

impl Default for Wallpaper {
    fn default() -> Self {
        Self {
            background: Background::Orbit,
            color: [18, 24, 29],
            dim: 35,
            photo: None,
        }
    }
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub wallpaper: Wallpaper,
    pub room_wallpapers: BTreeMap<String, Wallpaper>,
    pub drafts: BTreeMap<String, DraftState>,
    pub local_discovery: bool,
}

impl Preferences {
    pub fn load(root: &Path) -> Result<Self> {
        match fs::read(root.join("preferences.json")) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        fs::create_dir_all(root)?;
        let mut file = tempfile::NamedTempFile::new_in(root)?;
        serde_json::to_writer(&mut file, self)?;
        file.as_file().sync_all()?;
        file.persist(root.join("preferences.json"))
            .map_err(|error| error.error)?;
        Ok(())
    }
}

pub fn stage_file(root: &Path, source: &Path, limit: u64) -> Result<PathBuf> {
    let name = source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Dosya");
    let file = fs::File::open(source)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && (1..=limit).contains(&metadata.len()),
        "Dosya sınırı {:.0} MiB",
        limit as f64 / 1048576.
    );
    let dir = root.join("draft-assets").join(Uuid::new_v4().to_string());
    fs::create_dir_all(&dir)?;
    let mut output = tempfile::NamedTempFile::new_in(&dir)?;
    let written = std::io::copy(&mut file.take(limit + 1), &mut output)?;
    ensure!(
        (1..=limit).contains(&written),
        "Dosya kopyalanırken boyutu değişti"
    );
    output.as_file().sync_all()?;
    let target = dir.join(name);
    output
        .persist_noclobber(&target)
        .map_err(|error| error.error)?;
    Ok(target)
}

#[cfg(windows)]
pub fn wallpaper_image(root: &Path, source: &Path) -> Result<PathBuf> {
    let file = fs::File::open(source)?;
    ensure!(
        file.metadata()?.len() <= 20 * 1024 * 1024,
        "En fazla 20 MiB boyutunda bir fotoğraf seç"
    );
    let mut reader =
        image::ImageReader::new(std::io::BufReader::new(file)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(6400);
    limits.max_image_height = Some(6400);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode()?.thumbnail(1600, 1200).to_rgb8();
    let dir = root.join("wallpapers");
    fs::create_dir_all(&dir)?;
    let mut file = tempfile::NamedTempFile::new_in(&dir)?;
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut file, 87).encode_image(&image)?;
    file.as_file().sync_all()?;
    let path = dir.join(format!("{}.jpg", Uuid::new_v4()));
    file.persist_noclobber(&path).map_err(|error| error.error)?;
    Ok(path)
}

#[cfg(windows)]
pub fn clipboard_image(root: &Path, limit: u64) -> Result<Option<PathBuf>> {
    let mut clipboard = arboard::Clipboard::new()?;
    let image = match clipboard.get_image() {
        Ok(image) => image,
        Err(arboard::Error::ContentNotAvailable) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        image.width.saturating_mul(image.height) <= 32_000_000,
        "Pano resmi çok büyük"
    );
    let dir = root.join("draft-assets").join(Uuid::new_v4().to_string());
    fs::create_dir_all(&dir)?;
    let mut output = tempfile::NamedTempFile::new_in(&dir)?;
    image::ImageEncoder::write_image(
        image::codecs::png::PngEncoder::new(&mut output),
        &image.bytes,
        image.width.try_into()?,
        image.height.try_into()?,
        image::ExtendedColorType::Rgba8,
    )?;
    ensure!(
        output.as_file().metadata()?.len() <= limit,
        "Pano resmi dosya sınırını aşıyor"
    );
    output.flush()?;
    output.as_file().sync_all()?;
    let path = dir.join("Ekran görüntüsü.png");
    output
        .persist_noclobber(&path)
        .map_err(|error| error.error)?;
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_and_wallpaper_persist_independently_after_source_is_gone() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("profile");
        let source = temp.path().join("dosya.txt");
        fs::write(&source, "Taslak eki")?;
        let attached = stage_file(&root, &source, 1024)?;
        fs::remove_file(source)?;
        let mut preferences = Preferences::default();
        preferences.drafts.insert(
            "host/oda".into(),
            DraftState {
                text: "Sohbet taslağı".into(),
                reply: Some((17, "Deniz".into(), "Eski mesaj".into())),
                attachment: Some(attached.clone()),
            },
        );
        preferences.save(&root)?;
        preferences.room_wallpapers.insert(
            "host/oda".into(),
            Wallpaper {
                background: Background::Plain,
                dim: 12,
                ..Wallpaper::default()
            },
        );
        preferences.save(&root)?;
        let reloaded = Preferences::load(&root)?;
        assert!(reloaded == preferences);
        assert_eq!(fs::read_to_string(attached)?, "Taslak eki");
        assert!(!reloaded.drafts.contains_key("host/diğer"));
        Ok(())
    }
}
