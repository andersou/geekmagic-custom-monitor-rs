mod autostart;
mod config;
mod device;
mod plugin;
mod plugins;
mod render;
mod setup;
mod upload;
mod uninstall;

use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use image::RgbaImage;

use crate::device::Model;
use crate::plugin::Plugin;

#[derive(Parser)]
#[command(about = "Push extensible monitor screens to a GeekMagic display")]
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

    /// Manage auto-start at login (macOS launchd, Linux systemd user, Windows Run key)
    Boot {
        #[command(subcommand)]
        action: BootAction,
    },

    /// Disable auto-start and remove this binary, configuration, and backups
    Uninstall,
}

#[derive(Subcommand)]
enum BootAction {
    /// Install and enable auto-start
    Enable,
    /// Disable and remove auto-start
    Disable,
}

struct RuntimeArgs {
    host: Option<String>,
    interval: Option<u64>,
    output_dir: Option<String>,
    parallel: bool,
    autoplay_interval: u64,
    image_mode: upload::ImageMode,
    backup_retention: usize,
    model: Option<String>,
}

fn now() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// One collect+render pass over the plugin list; failures are per-plugin.
fn collect_render(plugins: &mut [Box<dyn Plugin>], parallel: bool) -> Vec<(&'static str, RgbaImage)> {
    let run_one = |p: &mut Box<dyn Plugin>| -> Result<(&'static str, RgbaImage)> {
        p.collect()?;
        let img = p.render()?;
        Ok((p.filename(), img))
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
        plugins.iter_mut().map(|p| run_one(p)).collect()
    };

    let mut screens = Vec::new();
    for (plugin, result) in plugins.iter().zip(results) {
        match result {
            Ok(screen) => screens.push(screen),
            Err(e) => eprintln!("[{}] plugin '{}' failed: {e:#}", now(), plugin.name()),
        }
    }
    screens
}

fn run_cycle(
    plugins: &mut [Box<dyn Plugin>],
    args: &RuntimeArgs,
    device: &mut Option<device::DeviceInfo>,
) -> Result<()> {
    // Re-probe when detection has not succeeded yet (device may have booted
    // after the daemon started).
    if let (Some(host), Some(d)) = (&args.host, device.as_mut()) {
        if d.model == Model::Unknown {
            let client = upload::make_client()?;
            *d = device::detect(&client, &format!("http://{host}"), args.model.as_deref());
            log_device(d);
        }
    }

    let screens = collect_render(plugins, args.parallel);
    if screens.is_empty() {
        eprintln!("[{}] all plugins failed; skipping upload this cycle", now());
        return Ok(());
    }

    if let Some(dir) = &args.output_dir {
        std::fs::create_dir_all(dir)?;
        for (filename, img) in &screens {
            let png = format!("{}.png", filename.trim_end_matches(".jpg"));
            let path = format!("{dir}/{png}");
            img.save(&path)?;
            println!("[{}] saved {path}", now());
        }
    } else {
        let host = args.host.as_ref().expect("host checked at startup");
        let album_theme = device.as_ref().map(|d| d.album_theme).unwrap_or(3);
        let refs: Vec<(&str, &RgbaImage)> =
            screens.iter().map(|(f, i)| (*f, i)).collect();
        upload::upload_screens(host, album_theme, args.autoplay_interval, &refs)?;
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

    let mut device = if args.output_dir.is_none() {
        let host = args.host.as_ref().expect("host checked above");
        let client = upload::make_client()?;
        let d = device::detect(&client, &format!("http://{host}"), args.model.as_deref());
        log_device(&d);
        let keep_filenames: Vec<_> = plugins.iter().map(|plugin| plugin.filename()).collect();
        upload::prepare_images(
            host,
            args.image_mode,
            args.backup_retention,
            &keep_filenames,
        )?;
        Some(d)
    } else {
        None
    };

    if let Some(interval) = args.interval {
        let interval = interval.max(10);
        if let Some(host) = &args.host {
            println!("Daemon mode: pushing every {interval}s to {host}");
        }
        loop {
            if let Err(e) = run_cycle(&mut plugins, &args, &mut device) {
                eprintln!("[{}] Error: {e:#}", now());
            }
            thread::sleep(Duration::from_secs(interval));
        }
    } else {
        run_cycle(&mut plugins, &args, &mut device)
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
            let backup_retention = match cfg.backup_retention {
                Some(0) => {
                    eprintln!("backup_retention must be at least 1, falling back to 5");
                    5
                }
                Some(retention) => retention,
                None => 5,
            };
            let args = RuntimeArgs {
                host: host.or(cfg.host.clone()),
                interval: if once { None } else { daemon.or(cfg.interval) },
                output_dir,
                parallel,
                autoplay_interval: cfg.autoplay_interval.unwrap_or(10),
                image_mode,
                backup_retention,
                model: cfg.model.clone(),
            };
            run(args, cfg)
        }
        Command::Setup { config } => setup::run(config.as_deref()),
        Command::Boot { action } => autostart::boot(matches!(action, BootAction::Enable)),
        Command::Uninstall => uninstall::run(),
    }
}
