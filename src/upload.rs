use std::{fs, io::Cursor, path::Path};

use anyhow::{Context, Result};
use chrono::Local;
use image::RgbaImage;
use reqwest::blocking::multipart;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageMode {
    Append,
    OnlyStats,
}

impl ImageMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "append" => Some(Self::Append),
            "only-stats" => Some(Self::OnlyStats),
            _ => None,
        }
    }
}

fn parse_filelist(body: &str) -> Vec<String> {
    if body.contains("/image//") {
        let mut names: Vec<_> = body
            .split("/image//")
            .skip(1)
            .filter_map(|chunk| {
                let name = chunk.split(['\'', '"', '<']).next().unwrap_or_default();
                (!name.is_empty()).then(|| name.to_string())
            })
            .collect();
        names.dedup();
        names
    } else {
        body.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    }
}

fn is_image(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, extension)| {
        extension.eq_ignore_ascii_case("jpg")
            || extension.eq_ignore_ascii_case("jpeg")
            || extension.eq_ignore_ascii_case("png")
            || extension.eq_ignore_ascii_case("gif")
    })
}

fn is_backup_dir(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let bytes = name.as_bytes();
    bytes.len() == 15
        && bytes[8] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 8 || byte.is_ascii_digit())
}

fn prune_backups(root: &Path, retention: usize) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }

    let mut backups: Vec<_> = fs::read_dir(root)
        .with_context(|| format!("failed to read backup dir {}", root.display()))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if !entry.file_type().ok()?.is_dir() || !is_backup_dir(&path) {
                return None;
            }
            let has_files = fs::read_dir(&path).ok()?.any(|child| {
                child
                    .ok()
                    .and_then(|child| child.file_type().ok())
                    .is_some_and(|file_type| file_type.is_file())
            });
            has_files.then_some(path)
        })
        .collect();
    backups.sort_unstable();

    let remove_count = backups.len().saturating_sub(retention.max(1));
    for path in backups.into_iter().take(remove_count) {
        fs::remove_dir_all(&path)
            .with_context(|| format!("failed to remove old backup {}", path.display()))?;
    }
    Ok(())
}

fn remove_non_screen_images(
    client: &reqwest::blocking::Client,
    base: &str,
    backup_retention: usize,
    keep_filenames: &[&str],
) -> Result<()> {
    let backup_root = crate::config::config_root()?.join("backups");
    if let Err(error) = prune_backups(&backup_root, backup_retention) {
        eprintln!("only-stats: failed to prune old backups: {error:#}");
    }
    let body = client
        .get(format!("{base}/filelist?dir=/image/"))
        .send()?
        .text()
        .unwrap_or_default();
    let victims: Vec<_> = parse_filelist(&body)
        .into_iter()
        .filter(|name| is_image(name) && !keep_filenames.contains(&name.as_str()))
        .collect();

    if victims.is_empty() {
        return Ok(());
    }

    let backup_dir = backup_root.join(Local::now().format("%Y%m%d-%H%M%S").to_string());
    let mut created_backup_dir = false;
    let mut backed_up = 0;
    let mut removed = 0;
    for name in victims {
        let bytes = match client
            .get(format!("{base}/image//{name}"))
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .and_then(reqwest::blocking::Response::bytes)
        {
            Ok(bytes) => bytes,
            Err(error) => {
                eprintln!("only-stats: failed to back up {name}: {error}; keeping device file");
                continue;
            }
        };

        if !backup_dir.exists() {
            fs::create_dir_all(&backup_dir)
                .with_context(|| format!("failed to create backup dir {}", backup_dir.display()))?;
            created_backup_dir = true;
        }

        let backup_path = backup_dir.join(&name);
        if let Err(error) = fs::write(&backup_path, &bytes) {
            eprintln!(
                "only-stats: failed to write backup {}: {error}; keeping device file",
                backup_path.display()
            );
            continue;
        }
        backed_up += 1;

        let mut delete_url = reqwest::Url::parse(&format!("{base}/delete"))?;
        delete_url
            .query_pairs_mut()
            .append_pair("file", &format!("/image//{name}"));
        match client
            .get(delete_url)
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
        {
            Ok(_) => removed += 1,
            Err(error) => eprintln!("only-stats: failed to delete {name}: {error}"),
        }
    }

    if backed_up == 0 {
        if created_backup_dir && let Err(error) = fs::remove_dir(&backup_dir) {
            eprintln!(
                "only-stats: failed to remove empty backup dir {}: {error}",
                backup_dir.display()
            );
        }
        return Ok(());
    }

    if let Err(error) = prune_backups(&backup_root, backup_retention) {
        eprintln!("only-stats: failed to prune old backups: {error:#}");
    }

    if removed > 0 {
        println!(
            "[{}] only-stats: removed {removed} image(s), backup at {}",
            Local::now().format("%H:%M:%S"),
            backup_dir.display()
        );
    }

    Ok(())
}

fn encode_jpeg(img: &RgbaImage) -> Result<Vec<u8>> {
    let rgb = image::DynamicImage::ImageRgba8(img.clone()).into_rgb8();
    let mut jpeg_buf = Cursor::new(Vec::new());
    rgb.write_to(&mut jpeg_buf, image::ImageFormat::Jpeg)?;
    Ok(jpeg_buf.into_inner())
}

fn upload_file(
    client: &reqwest::blocking::Client,
    base: &str,
    filename: &str,
    jpeg_bytes: Vec<u8>,
) -> Result<()> {
    let part = multipart::Part::bytes(jpeg_bytes)
        .file_name(filename.to_string())
        .mime_str("image/jpeg")?;
    let form = multipart::Form::new().part("file", part);

    let resp = client
        .post(format!("{base}/doUpload?dir=/image/"))
        .multipart(form)
        .send();

    // Firmware quirk: successful uploads can return technically invalid HTTP.
    match resp {
        Ok(_) => {}
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("Duplicate Content-Length")
                || msg.contains("Data after")
                || msg.contains("invalid content-length")
            {
            } else {
                return Err(e).context("upload failed");
            }
        }
    }
    Ok(())
}

pub fn make_client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?)
}

/// Apply the configured image policy once during process initialization.
pub fn prepare_images(
    host: &str,
    mode: ImageMode,
    backup_retention: usize,
    keep_filenames: &[&str],
) -> Result<()> {
    if mode == ImageMode::Append {
        return Ok(());
    }

    let base = format!("http://{host}");
    let client = make_client()?;
    remove_non_screen_images(&client, &base, backup_retention, keep_filenames)
}

/// Upload every screen, switch the device to its Photo Album theme and enable
/// autoplay. Sequential only — the ESP8266 handles one request at a time.
pub fn upload_screens(
    host: &str,
    album_theme: u8,
    autoplay_interval: u64,
    screens: &[(&str, &RgbaImage)],
) -> Result<()> {
    let base = format!("http://{host}");
    let client = make_client()?;
    for (filename, img) in screens {
        upload_file(&client, &base, filename, encode_jpeg(img)?)?;
    }

    client
        .get(format!("{base}/set?theme={album_theme}"))
        .send()
        .context("failed to set theme")?;
    if let Some((first, _)) = screens.first() {
        client
            .get(format!("{base}/set?img=/image//{first}"))
            .send()
            .context("failed to set image")?;
    }

    client
        .get(format!("{base}/set?i_i={autoplay_interval}&autoplay=1"))
        .send()
        .context("failed to enable autoplay")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_filelist, prune_backups};
    use std::fs;

    #[test]
    fn parses_pro_html_filelist() {
        let body = "<a href='/image//bootxp.gif'>bootxp.gif</a>\
                    <input onclick=\"deletef('/image//bootxp.gif')\">\
                    <a href=\"/image//stats.jpg\">stats.jpg</a>";

        assert_eq!(parse_filelist(body), ["bootxp.gif", "stats.jpg"]);
    }

    #[test]
    fn parses_bare_line_filelist() {
        let body = "bootxp.gif\n\n stats.jpg \r\n";

        assert_eq!(parse_filelist(body), ["bootxp.gif", "stats.jpg"]);
    }

    #[test]
    fn keeps_configured_number_of_newest_backup_directories() {
        let root = std::env::temp_dir().join(format!(
            "geekmagic-backup-retention-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let empty = root.join("20260731-120000");
        fs::create_dir_all(&empty).unwrap();

        for day in 1..=7 {
            let backup = root.join(format!("202608{day:02}-120000"));
            fs::create_dir_all(&backup).unwrap();
            fs::write(backup.join("image.jpg"), [day]).unwrap();
        }
        let unrelated = root.join("manual");
        fs::create_dir_all(&unrelated).unwrap();
        fs::write(unrelated.join("keep.txt"), b"keep").unwrap();

        prune_backups(&root, 3).unwrap();
        assert!(empty.exists());

        for day in 1..=4 {
            assert!(!root.join(format!("202608{day:02}-120000")).exists());
        }
        for day in 5..=7 {
            assert!(root.join(format!("202608{day:02}-120000")).exists());
        }
        assert!(unrelated.exists());

        fs::remove_dir_all(root).unwrap();
    }
}
