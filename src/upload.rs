use std::{fs, path::Path};

use anyhow::{Context, Result};
use chrono::Local;
use image::{
    ExtendedColorType, ImageEncoder, RgbaImage,
    codecs::{jpeg::JpegEncoder, png::PngEncoder},
};
use reqwest::blocking::multipart;

pub const DEFAULT_JPEG_QUALITY: u8 = 75;
pub const GENERATED_FILE_PREFIX: &str = "gmcm-plugin-";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Jpeg,
    Png,
}

impl OutputFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "jpg" => Some(Self::Jpeg),
            "png" => Some(Self::Png),
            _ => None,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
        }
    }

    pub fn mime_type(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
        }
    }
}


const LEGACY_GENERATED_FILENAMES: &[&str] = &["claude.jpg", "codex.jpg", "disk.jpg", "kimi.jpg"];

fn select_cleanup_victims(
    file_names: Vec<String>,
    mode: ImageMode,
    keep_filenames: &[String],
) -> Vec<String> {
    file_names
        .into_iter()
        .filter(|name| {
            is_image(name)
                && !keep_filenames.contains(name)
                && (mode == ImageMode::OnlyStats
                    || name.starts_with(GENERATED_FILE_PREFIX)
                    || LEGACY_GENERATED_FILENAMES.contains(&name.as_str()))
        })
        .collect()
}
pub fn generated_filename(plugin_name: &str, format: OutputFormat) -> String {
    format!("{GENERATED_FILE_PREFIX}{plugin_name}.{}", format.extension())
}
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

fn cleanup_images(
    client: &reqwest::blocking::Client,
    base: &str,
    mode: ImageMode,
    backup_retention: usize,
    keep_filenames: &[String],
) -> Result<()> {
    let backup_root = crate::config::config_root()?.join("backups");
    if let Err(error) = prune_backups(&backup_root, backup_retention) {
        eprintln!("image cleanup: failed to prune old backups: {error:#}");
    }
    let body = client
        .get(format!("{base}/filelist?dir=/image/"))
        .send()?
        .error_for_status()?
        .text()
        .context("failed to read image file list")?;
    let victims = select_cleanup_victims(parse_filelist(&body), mode, keep_filenames);

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
                eprintln!("image cleanup: failed to back up {name}: {error}; keeping device file");
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
                "image cleanup: failed to write backup {}: {error}; keeping device file",
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
            Err(error) => eprintln!("image cleanup: failed to delete {name}: {error}"),
        }
    }

    if backed_up == 0 {
        if created_backup_dir && let Err(error) = fs::remove_dir(&backup_dir) {
            eprintln!(
                "image cleanup: failed to remove empty backup dir {}: {error}",
                backup_dir.display()
            );
        }
        return Ok(());
    }

    if let Err(error) = prune_backups(&backup_root, backup_retention) {
        eprintln!("image cleanup: failed to prune old backups: {error:#}");
    }

    if removed > 0 {
        println!(
            "[{}] image cleanup: removed {removed} image(s), backup at {}",
            Local::now().format("%H:%M:%S"),
            backup_dir.display()
        );
    }

    Ok(())
}

pub fn encode_image(
    img: &RgbaImage,
    format: OutputFormat,
    jpeg_quality: u8,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    match format {
        OutputFormat::Jpeg => {
            JpegEncoder::new_with_quality(&mut bytes, jpeg_quality).encode_image(img)?;
        }
        OutputFormat::Png => {
            PngEncoder::new(&mut bytes).write_image(
                img.as_raw(),
                img.width(),
                img.height(),
                ExtendedColorType::Rgba8,
            )?;
        }
    }
    Ok(bytes)
}

fn upload_file(
    client: &reqwest::blocking::Client,
    base: &str,
    filename: &str,
    image_bytes: Vec<u8>,
    format: OutputFormat,
) -> Result<()> {
    let part = multipart::Part::bytes(image_bytes)
        .file_name(filename.to_string())
        .mime_str(format.mime_type())?;
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
    output_format: OutputFormat,
    plugin_names: &[&str],
) -> Result<()> {
    let base = format!("http://{host}");
    let client = make_client()?;
    let keep_filenames: Vec<_> = plugin_names
        .iter()
        .map(|name| generated_filename(name, output_format))
        .collect();
    cleanup_images(&client, &base, mode, backup_retention, &keep_filenames)
}
/// Upload every screen, switch the device to its Photo Album theme and enable
/// autoplay. Sequential only — the ESP8266 handles one request at a time.
pub fn upload_screens(
    host: &str,
    album_theme: u8,
    autoplay_interval: u64,
    output_format: OutputFormat,
    jpeg_quality: u8,
    screens: &[(&str, &RgbaImage)],
) -> Result<()> {
    let base = format!("http://{host}");
    let client = make_client()?;
    for (plugin_name, img) in screens {
        let filename = generated_filename(plugin_name, output_format);
        upload_file(
            &client,
            &base,
            &filename,
            encode_image(img, output_format, jpeg_quality)?,
            output_format,
        )?;
    }

    client
        .get(format!("{base}/set?theme={album_theme}"))
        .send()
        .context("failed to set theme")?;
    if let Some((first_plugin, _)) = screens.first() {
        let filename = generated_filename(first_plugin, output_format);
        client
            .get(format!("{base}/set?img=/image//{filename}"))
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
    use std::{
        fs,
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        thread,
    };

    use image::{GenericImageView, Rgba, RgbaImage};

    use super::{
        ImageMode, OutputFormat, encode_image, generated_filename, parse_filelist, prepare_images,
        prune_backups, select_cleanup_victims, upload_screens,
    };

    fn read_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        let mut expected_length = None;
        loop {
            let read = stream.read(&mut buffer).unwrap();
            assert_ne!(read, 0, "client closed before a complete request");
            bytes.extend_from_slice(&buffer[..read]);
            if expected_length.is_none()
                && let Some(headers_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
            {
                let headers = String::from_utf8_lossy(&bytes[..headers_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length: ")
                            .or_else(|| line.strip_prefix("Content-Length: "))
                    })
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0);
                expected_length = Some(headers_end + 4 + content_length);
            }
            if expected_length.is_some_and(|length| bytes.len() >= length) {
                return String::from_utf8_lossy(&bytes).into_owned();
            }
        }
    }

    fn test_server(responses: Vec<&str>) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let responses: Vec<String> = responses.into_iter().map(str::to_string).collect();
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                stream.write_all(response.as_bytes()).unwrap();
                requests.push(request);
            }
            requests
        });
        (host, handle)
    }

    #[test]
    fn output_format_parses_only_supported_literals() {
        assert_eq!(OutputFormat::parse("jpg"), Some(OutputFormat::Jpeg));
        assert_eq!(OutputFormat::parse("png"), Some(OutputFormat::Png));
        assert_eq!(OutputFormat::parse("jpeg"), None);
        assert_eq!(OutputFormat::parse("JPG"), None);
        assert_eq!(OutputFormat::parse(""), None);
    }

    #[test]
    fn output_formats_encode_expected_bytes_and_names() {
        let image = RgbaImage::from_pixel(2, 1, Rgba([10, 20, 30, 40]));
        let jpeg = encode_image(&image, OutputFormat::Jpeg, 75).unwrap();
        assert_eq!(&jpeg[..3], [0xFF, 0xD8, 0xFF]);
        assert_eq!(image::load_from_memory(&jpeg).unwrap().dimensions(), (2, 1));
        assert_eq!(OutputFormat::Jpeg.mime_type(), "image/jpeg");
        assert_eq!(generated_filename("codex", OutputFormat::Jpeg), "gmcm-plugin-codex.jpg");

        let png = encode_image(&image, OutputFormat::Png, 75).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(image::load_from_memory(&png).unwrap().to_rgba8().get_pixel(0, 0), &Rgba([10, 20, 30, 40]));
        assert_eq!(OutputFormat::Png.mime_type(), "image/png");
        assert_eq!(generated_filename("codex", OutputFormat::Png), "gmcm-plugin-codex.png");
    }

    #[test]
    fn jpeg_quality_changes_encoded_payload() {
        let image = RgbaImage::from_fn(64, 64, |x, y| Rgba([x as u8 * 4, y as u8 * 4, (x ^ y) as u8 * 4, 255]));
        let low = encode_image(&image, OutputFormat::Jpeg, 25).unwrap();
        let high = encode_image(&image, OutputFormat::Jpeg, 90).unwrap();
        assert_eq!(image::load_from_memory(&low).unwrap().dimensions(), (64, 64));
        assert_eq!(image::load_from_memory(&high).unwrap().dimensions(), (64, 64));
        assert_ne!(low, high);
    }

    #[test]
    fn append_cleanup_selects_stale_generated_and_legacy_files() {
        let keep = vec!["gmcm-plugin-codex.png".to_string()];
        let files = ["gmcm-plugin-codex.png", "gmcm-plugin-codex.jpg", "gmcm-plugin-kimi.png", "codex.jpg", "family.jpg", "bootxp.gif"]
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(
            select_cleanup_victims(files, ImageMode::Append, &keep),
            ["gmcm-plugin-codex.jpg", "gmcm-plugin-kimi.png", "codex.jpg"]
        );
    }

    #[test]
    fn only_stats_cleanup_selects_every_non_kept_image() {
        let keep = vec!["gmcm-plugin-codex.png".to_string()];
        let files = ["gmcm-plugin-codex.png", "gmcm-plugin-codex.jpg", "gmcm-plugin-kimi.png", "codex.jpg", "family.jpg", "bootxp.gif", "notes.txt"]
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(
            select_cleanup_victims(files, ImageMode::OnlyStats, &keep),
            ["gmcm-plugin-codex.jpg", "gmcm-plugin-kimi.png", "codex.jpg", "family.jpg", "bootxp.gif"]
        );
    }

    #[test]
    fn upload_protocol_uses_generated_png_name() {
        let response = "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
        let (host, server) = test_server(vec![response; 4]);
        let image = RgbaImage::from_pixel(1, 1, Rgba([1, 2, 3, 4]));
        upload_screens(&host, 3, 10, OutputFormat::Png, 80, &[("codex", &image)]).unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[0].contains("filename=\"gmcm-plugin-codex.png\""));
        assert!(requests[0].contains("Content-Type: image/png"));
        assert!(requests[2].starts_with("GET /set?img=/image//gmcm-plugin-codex.png HTTP/1.1"));
    }

    #[test]
    fn cleanup_aborts_on_filelist_http_error() {
        let response = "HTTP/1.1 500 Internal Server Error\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
        let (host, server) = test_server(vec![response]);
        assert!(prepare_images(&host, ImageMode::Append, 5, OutputFormat::Png, &["codex"]).is_err());
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /filelist?dir=/image/ HTTP/1.1"));
        assert!(!requests.iter().any(|request| request.contains("/delete")));
    }

    #[test]
    fn parses_pro_html_filelist() {
        let body = "<a href='/image//bootxp.gif'>bootxp.gif</a>\
                    <input onclick=\"deletef('/image//bootxp.gif')\">\
                    <a href=\"/image//stats.jpg\">stats.jpg</a>";
        assert_eq!(parse_filelist(body), ["bootxp.gif", "stats.jpg"]);
    }

    #[test]
    fn parses_bare_line_filelist() {
        assert_eq!(parse_filelist("bootxp.gif\n\n stats.jpg \r\n"), ["bootxp.gif", "stats.jpg"]);
    }

    #[test]
    fn keeps_configured_number_of_newest_backup_directories() {
        let root = std::env::temp_dir().join(format!("geekmagic-backup-retention-test-{}", std::process::id()));
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
