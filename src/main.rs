mod config;
mod daemon;
mod device;
mod plugin;
mod plugins;
mod render;
mod setup;
mod status;
mod uninstall;
mod upload;

use std::collections::HashMap;
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
use image::RgbaImage;

use crate::device::Model;
use crate::plugin::UiPlugin;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(
    name = "geekmagic-monitors",
    version,
    about = "Push extensible monitor screens to a GeekMagic display"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Collect, render and push (or save) plugin screens
    Run {
        /// GeekMagic device IP address
        #[arg(long)]
        host: Option<String>,

        /// Path to config file
        #[arg(long)]
        config: Option<String>,

        /// Run as daemon, pushing every N seconds
        #[arg(short, long)]
        daemon: Option<u64>,
        /// Single cycle: use the config but ignore its interval
        #[arg(long)]
        once: bool,

        /// Save rendered images to this directory instead of uploading
        #[arg(long)]
        output_dir: Option<String>,

        /// Collect+render plugins in parallel
        #[arg(long, overrides_with = "no_parallel")]
        parallel: bool,

        /// Force sequential collect+render
        #[arg(long)]
        no_parallel: bool,
    },

    /// Interview: write the config file (Enter accepts the bracketed default)
    Setup {
        /// Path to config file
        #[arg(long)]
        config: Option<String>,
    },

    /// Manage the background daemon (auto-start at login, status, restart)
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },

    /// Disable the daemon and remove this binary, configuration, and backups
    Uninstall,
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Install and enable auto-start at login
    Enable,
    /// Disable and remove auto-start
    Disable,
    /// Show service state and the last cycle's outcome
    Status,
    /// Restart the running daemon
    Restart,
}

struct RuntimeArgs {
    host: Option<String>,
    interval: Option<u64>,
    output_dir: Option<String>,
    parallel: bool,
    autoplay_interval: u64,
    image_mode: upload::ImageMode,
    output_format: upload::OutputFormat,
    jpeg_quality: u8,
    backup_retention: usize,
    model: Option<String>,
    failure_threshold: u32,
}

fn now() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

fn record_outcome(
    failures: &mut HashMap<&'static str, u32>,
    plugin: &dyn UiPlugin,
    result: Result<(&'static str, RgbaImage)>,
    threshold: u32,
    report: &mut CycleReport,
) -> Option<(&'static str, RgbaImage)> {
    match result {
        Ok(screen) => {
            failures.remove(plugin.name());
            report.succeeded.push(plugin.name().to_string());
            Some(screen)
        }
        Err(error) => {
            report.failed.push(status::PluginFailure {
                plugin: plugin.name().to_string(),
                error: format!("{error:#}"),
            });
            let count = {
                let count = failures.entry(plugin.name()).or_insert(0);
                *count = (*count).saturating_add(1);
                *count
            };
            eprintln!(
                "[{}] plugin '{}' failed ({count}/{threshold}): {error:#}",
                now(),
                plugin.name()
            );
            if count < threshold {
                return None;
            }
            if count == threshold {
                eprintln!(
                    "[{}] plugin '{}': circuit breaker on, showing error screen",
                    now(),
                    plugin.name()
                );
            }
            Some((
                plugin.name(),
                crate::render::error::render_plugin_error(plugin.name(), count),
            ))
        }
    }
}

#[derive(Default)]
struct CycleReport {
    screens: Vec<(&'static str, RgbaImage)>,
    succeeded: Vec<String>,
    failed: Vec<status::PluginFailure>,
}

/// One collect+render pass over the plugin list; failures are per-plugin.
fn collect_render(
    plugins: &mut [Box<dyn UiPlugin>],
    parallel: bool,
    failures: &mut HashMap<&'static str, u32>,
    threshold: u32,
) -> CycleReport {
    let run_one = |p: &mut Box<dyn UiPlugin>| -> Result<(&'static str, RgbaImage)> {
        p.collect()?;
        let img = p.render()?;
        Ok((p.name(), img))
    };

    let results: Vec<Result<(&'static str, RgbaImage)>> = if parallel {
        thread::scope(|s| {
            let handles: Vec<_> = plugins
                .iter_mut()
                .map(|p| s.spawn(move || run_one(p)))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| Err(anyhow!("plugin panicked"))))
                .collect()
        })
    } else {
        plugins.iter_mut().map(run_one).collect()
    };

    let mut report = CycleReport::default();
    for (plugin, result) in plugins.iter().zip(results) {
        if let Some(screen) = record_outcome(failures, &**plugin, result, threshold, &mut report) {
            report.screens.push(screen);
        }
    }
    report
}

fn run_cycle(
    plugins: &mut [Box<dyn UiPlugin>],
    args: &RuntimeArgs,
    device: &mut Option<device::DeviceInfo>,
    failures: &mut HashMap<&'static str, u32>,
    status: &mut status::DaemonStatus,
) -> Result<()> {
    status.last_cycle_at = Some(chrono::Local::now().to_rfc3339());
    // Re-probe when detection has not succeeded yet (device may have booted
    // after the daemon started).
    if let (Some(host), Some(d)) = (&args.host, device.as_mut())
        && d.model == Model::Unknown
    {
        let client = upload::make_client()?;
        *d = device::detect(&client, &format!("http://{host}"), args.model.as_deref());
        log_device(d);
    }

    let report = collect_render(plugins, args.parallel, failures, args.failure_threshold);
    let screens = report.screens;
    status.succeeded = report.succeeded;
    status.failed = report.failed;
    status.device = Some(device_label(device));
    if screens.is_empty() {
        eprintln!("[{}] all plugins failed; skipping upload this cycle", now());
        status.upload = Some("skipped: all plugins failed".to_string());
        return Ok(());
    }

    if let Some(dir) = &args.output_dir {
        std::fs::create_dir_all(dir)?;
        for (plugin_name, img) in &screens {
            let filename = upload::generated_filename(plugin_name, args.output_format);
            let path = format!("{dir}/{filename}");
            let bytes = upload::encode_image(img, args.output_format, args.jpeg_quality)?;
            std::fs::write(&path, bytes)?;
            println!("[{}] saved {path}", now());
        }
        status.upload = Some(format!("saved {} screen(s) to {dir}", screens.len()));
    } else {
        let host = args.host.as_ref().expect("host checked at startup");
        let album_theme = device.as_ref().map(|d| d.album_theme).unwrap_or(3);
        let refs: Vec<(&str, &RgbaImage)> =
            screens.iter().map(|(name, image)| (*name, image)).collect();
        if let Err(e) = upload::upload_screens(
            host,
            album_theme,
            args.autoplay_interval,
            args.output_format,
            args.jpeg_quality,
            &refs,
        ) {
            status.upload = Some(format!("failed: {e:#}"));
            return Err(e);
        }
        status.upload = Some(format!("pushed {} screen(s) to {host}", screens.len()));
        println!("[{}] pushed {} screen(s) to {host}", now(), screens.len());
    }
    Ok(())
}

fn log_device(d: &device::DeviceInfo) {
    match &d.firmware {
        Some(fw) => println!("Device: {fw}, album theme {}", d.album_theme),
        None => println!(
            "device detection failed (model {:?}), will retry each cycle",
            d.model
        ),
    }
}

fn device_label(device: &Option<device::DeviceInfo>) -> String {
    match device {
        Some(d) => d
            .firmware
            .clone()
            .unwrap_or_else(|| "detection failed (will retry)".to_string()),
        None => "not used (output-dir)".to_string(),
    }
}

fn run(args: RuntimeArgs, cfg: config::AppConfig) -> Result<()> {
    if args.host.is_none() && args.output_dir.is_none() {
        return Err(anyhow!("missing host; pass --host or set host in config"));
    }

    let mut plugins = plugins::registry(&cfg);
    if plugins.is_empty() {
        return Err(anyhow!("no plugins enabled"));
    }
    let names: Vec<&str> = plugins.iter().map(|p| p.name()).collect();
    println!("enabled plugins: {}", names.join(", "));

    let mut status = status::DaemonStatus {
        version: VERSION.to_string(),
        pid: std::process::id(),
        started_at: chrono::Local::now().to_rfc3339(),
        interval_secs: args.interval,
        plugins: names.iter().map(|n| n.to_string()).collect(),
        ..Default::default()
    };

    let mut device = if args.output_dir.is_none() {
        let host = args.host.as_ref().expect("host checked above");
        let client = upload::make_client()?;
        let d = device::detect(&client, &format!("http://{host}"), args.model.as_deref());
        log_device(&d);
        let plugin_names: Vec<_> = plugins.iter().map(|plugin| plugin.name()).collect();
        upload::prepare_images(
            host,
            args.image_mode,
            args.backup_retention,
            args.output_format,
            &plugin_names,
        )?;
        Some(d)
    } else {
        None
    };

    let mut failures = HashMap::new();
    if let Some(interval) = args.interval {
        let interval = interval.max(10);
        if let Some(host) = &args.host {
            println!("Daemon mode: pushing every {interval}s to {host}");
        }
        loop {
            let result = run_cycle(&mut plugins, &args, &mut device, &mut failures, &mut status);
            match result {
                Ok(()) => status.cycle_error = None,
                Err(error) => {
                    status.cycle_error = Some(format!("{error:#}"));
                    eprintln!("[{}] Error: {error:#}", now());
                }
            }
            status::write(&status);
            thread::sleep(Duration::from_secs(interval));
        }
    } else {
        let result = run_cycle(&mut plugins, &args, &mut device, &mut failures, &mut status);
        match &result {
            Ok(()) => status.cycle_error = None,
            Err(error) => status.cycle_error = Some(format!("{error:#}")),
        }
        status::write(&status);
        result
    }
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Command::Run {
            host,
            config: config_path,
            daemon,
            once,
            output_dir,
            parallel,
            no_parallel,
        } => {
            let cfg = config::load(config_path.as_deref())?;
            let parallel = if parallel {
                true
            } else if no_parallel {
                false
            } else {
                cfg.parallel_render.unwrap_or(false)
            };
            let image_mode = cfg
                .image_mode
                .as_deref()
                .map(|s| {
                    upload::ImageMode::parse(s).unwrap_or_else(|| {
                        eprintln!("unknown image_mode '{s}', falling back to 'append'");
                        upload::ImageMode::Append
                    })
                })
                .unwrap_or(upload::ImageMode::Append);
            let output_format = cfg
                .image_format
                .as_deref()
                .map(|value| {
                    upload::OutputFormat::parse(value).unwrap_or_else(|| {
                        eprintln!("unknown image_format '{value}', falling back to 'jpg'");
                        upload::OutputFormat::Jpeg
                    })
                })
                .unwrap_or(upload::OutputFormat::Jpeg);
            let jpeg_quality = match cfg.jpeg_quality {
                Some(value @ 1..=100) => value as u8,
                Some(value) => {
                    eprintln!(
                        "jpeg_quality '{value}' must be between 1 and 100, falling back to {}",
                        upload::DEFAULT_JPEG_QUALITY
                    );
                    upload::DEFAULT_JPEG_QUALITY
                }
                None => upload::DEFAULT_JPEG_QUALITY,
            };
            let backup_retention = match cfg.backup_retention {
                Some(0) => {
                    eprintln!("backup_retention must be at least 1, falling back to 5");
                    5
                }
                Some(retention) => retention,
                None => 5,
            };
            let failure_threshold = match cfg.failure_threshold {
                Some(0) => {
                    eprintln!(
                        "failure_threshold must be at least 1, falling back to {}",
                        config::DEFAULT_FAILURE_THRESHOLD
                    );
                    config::DEFAULT_FAILURE_THRESHOLD
                }
                Some(threshold) => threshold,
                None => config::DEFAULT_FAILURE_THRESHOLD,
            };
            let args = RuntimeArgs {
                host: host.or(cfg.host.clone()),
                interval: if once { None } else { daemon.or(cfg.interval) },
                output_dir,
                parallel,
                autoplay_interval: cfg.autoplay_interval.unwrap_or(10),
                image_mode,
                output_format,
                jpeg_quality,
                backup_retention,
                model: cfg.model.clone(),
                failure_threshold,
            };
            run(args, cfg)
        }
        Command::Setup { config } => setup::run(config.as_deref()),
        Command::Daemon { action } => match action {
            DaemonAction::Enable => daemon::enable(),
            DaemonAction::Disable => daemon::disable(),
            DaemonAction::Status => daemon::status(),
            DaemonAction::Restart => daemon::restart(),
        },
        Command::Uninstall => uninstall::run(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{Plugin, PluginKind};

    struct Stub;

    impl Plugin for Stub {
        fn name(&self) -> &'static str {
            "stub"
        }

        fn get_plugin_kind(&self) -> PluginKind {
            PluginKind::Ui
        }
    }

    impl UiPlugin for Stub {
        fn collect(&mut self) -> Result<()> {
            unreachable!("record_outcome does not collect")
        }

        fn render(&self) -> Result<RgbaImage> {
            unreachable!("record_outcome does not render")
        }
    }

    #[test]
    fn version_output_matches_package_version() {
        let Err(error) = Cli::try_parse_from(["geekmagic-monitors", "--version"]) else {
            panic!("--version must request Clap's version display");
        };
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
        assert_eq!(
            error.to_string(),
            format!("geekmagic-monitors {}\n", env!("CARGO_PKG_VERSION"))
        );
    }
    #[test]
    fn circuit_breaker_shows_error_screen_at_threshold_and_recovers() {
        let plugin = Stub;
        let mut failures = HashMap::new();
        let mut report = CycleReport::default();
        for _ in 0..4 {
            assert!(
                record_outcome(
                    &mut failures,
                    &plugin,
                    Err(anyhow!("failed")),
                    5,
                    &mut report
                )
                .is_none()
            );
        }
        let (_, image) = record_outcome(
            &mut failures,
            &plugin,
            Err(anyhow!("failed")),
            5,
            &mut report,
        )
        .expect("threshold failure must render an error screen");
        assert_eq!(image.dimensions(), (240, 240));
        assert_eq!(failures["stub"], 5);
        assert!(
            record_outcome(
                &mut failures,
                &plugin,
                Err(anyhow!("failed")),
                5,
                &mut report
            )
            .is_some()
        );
        assert_eq!(failures["stub"], 6);
        let screen = RgbaImage::new(240, 240);
        assert!(
            record_outcome(&mut failures, &plugin, Ok(("stub", screen)), 5, &mut report).is_some()
        );
        assert!(!failures.contains_key("stub"));
        assert_eq!(report.failed.len(), 6);
        assert_eq!(report.succeeded, ["stub"]);
    }
}
