# geekmagic-custom-monitor-rs

Extensible Rust monitors for GeekMagic SmallTV displays. The program collects metrics, renders images, and uploads them to the device in album mode.

Based on [geekmagic-stats](https://github.com/jimmystridh/geekmagic-stats).

## Features

- Built-in plugins for Claude Code usage, Codex ChatGPT-plan quota, Kimi Code plan quota, and local disk usage.
- Automatic SmallTV Ultra and SmallTV Pro detection.
- One-shot or periodic execution.
- Optional parallel rendering.
- Automatic startup integration for macOS, Linux, and Windows.
- Two album image modes:
  - `append`: preserves existing images and adds or updates the application's screens.
  - `only-stats`: at process startup, backs up and removes images that do not belong to the application.
- Configurable backup retention; the newest 5 backups are kept by default.

## Requirements

- Rust with Cargo.
- A GeekMagic SmallTV reachable on the local network.
- The Codex screen requires the official [Codex CLI](https://developers.openai.com/codex/) and ChatGPT login via `codex login`; API-key login does not expose subscription quotas. The binary is searched on `PATH`, `~/.local/bin/codex`, `/opt/homebrew/bin/codex`, and `/usr/local/bin/codex`; set `CODEX_BINARY` to use an explicit path.

## Installation

Install from this repository:

```sh
cargo install --path .
```

Or build an optimized binary without installing it:

```sh
cargo build --release
```

## Initial configuration

Run the configuration interview:

```sh
geekmagic-monitors setup
```

By default, the configuration is written to:

```text
~/.config/geekmagic-custom-monitors/config.toml
```

You can use a different path:

```sh
geekmagic-monitors setup --config /path/to/config.toml
```

Example:

```toml
host = "192.168.1.201"
model = "auto"                  # auto | ultra | pro
interval = 300                  # omit for one-shot execution
parallel_render = false
autoplay_interval = 10
image_mode = "append"           # append | only-stats
backup_retention = 5            # number of backup directories to keep
failure_threshold = 5            # consecutive failed cycles before an error screen

[plugins.claude]
enabled = true

[plugins.codex]
enabled = true

[plugins.disk]
enabled = true

[plugins.kimi]
enabled = true
# api_key = "sk-kimi-..."       # optional: else KIMI_CODE_API_KEY, KIMI_API_KEY, or a Kimi Code CLI login
```

Backups created by `only-stats` are stored in:

```text
~/.config/geekmagic-custom-monitors/backups/
```

A device file is deleted only after its local backup has been saved successfully. Cleanup and backup happen once during process startup; later update cycles only render and upload screens. If there is no file that can be saved, no empty backup directory remains or counts toward retention. `backup_retention` must be at least `1`.

`failure_threshold` is process-local and defaults to `5`. A plugin is omitted while its consecutive collect/render failures remain below the threshold; at and above it, the app uploads a native error screen under that plugin's filename while continuing to collect it every cycle. The first successful collect+render resets the count. Set `failure_threshold = 1` to emit the error screen for a failing one-shot run.

## Process flow

```text
+------------------------------------------------------------------+
|                                                                  |
|                          Process starts                          |
|                                                                  |
+------------------------------------------------------------------+
                                  v
+------------------------------------------------------------------+
|                                                                  |
|                      Load TOML and plugins                       |
|                                                                  |
+------------------------------------------------------------------+
                                  v
+------------------------------------------------------------------+
|                                                                  |
|                        Detect the device                         |
|                                                                  |
+------------------------------------------------------------------+
                                  v
+------------------------------------------------------------------+
|                                                                  |
|                     Apply image policy once                      |
|                                                                  |
+------------------------------------------------------------------+
                                  v
+------------------------------------------------------------------+
|                                                                  |
| append leaves album unchanged / only-stats backs up then deletes |
|                                                                  |
+------------------------------------------------------------------+
                                  v
+------------------------------------------------------------------+
|                                                                  |
|           only-stats keeps newest N backup directories           |
|                                                                  |
+------------------------------------------------------------------+
                                  v
+------------------------------------------------------------------+
|                                                                  |
|           Update loop: collect, render, upload, sleep            |
|                                                                  |
+------------------------------------------------------------------+
                                  v
+------------------------------------------------------------------+
|                                                                  |
|               Later cycles never back up or clean                |
|                                                                  |
+------------------------------------------------------------------+
```

## Plugin architecture

```text
+----------------------------------------------------------------+
|     src/plugin.rs - PluginKind, Plugin and UiPlugin traits     |
+----------------------------------------------------------------+
                                v
+----------------------------------------------------------------+
|           src/plugins/mod.rs - catalog and registry            |
+----------------------------------------------------------------+
         v----------------------v----------------------v----------------------v
+------------------+   +------------------+   +------------------+   +------------------+
|  claude/mod.rs   |   |   codex/mod.rs   |   |   kimi/mod.rs    |   |    disk/mod.rs   |
|   collect + Ui   |   |   collect + Ui   |   |   collect + Ui   |   |   collect + Ui   |
+------------------+   +------------------+   +------------------+   +------------------+
         |                      |                      |                      v
         +----------------------+---------uses---------+           +------------------+
                                |                                  |  disk/render.rs  |
                                v                                  | render disk.jpg  |
+-------------------------------------+                            +------------------+
|    plugins/agents_usage_ui/mod.rs   |                                      |
| renderer: claude.jpg, codex.jpg and |                                      |
|             kimi.jpg                |                                      |
+-------------------------------------+                                      |
                    |                                                        |
                    v                                                        |
+-------------------------------------+                                      |
|  src/render/common.rs - primitives  |<----------------uses-----------------+
+-------------------------------------+
```

The contract is defined in `src/plugin.rs`. A plugin's kind is part of the contract, so the registry, the generated configuration, and the `setup` interview all derive from the plugin list itself:

```rust
pub enum PluginKind {
    Ui,       // one 240x240 screen, toggleable under [plugins.<name>]
    Renderer, // shared drawing code imported by UI plugins
}

pub trait Plugin: Send {
    fn name(&self) -> &'static str;
    fn get_plugin_kind(&self) -> PluginKind;
    fn needs_api_key(&self) -> bool {
        false
    }
}

pub trait UiPlugin: Plugin {
    fn filename(&self) -> &'static str;
    fn collect(&mut self) -> anyhow::Result<()>;
    fn render(&self) -> anyhow::Result<image::RgbaImage>;
    fn depends_on(&self) -> &'static [&'static str] {
        &[]
    }
}
```

- `name()` is the key used by `[plugins.<name>]` in TOML.
- `get_plugin_kind()` decides whether the plugin owns a screen. Only `Ui` plugins are enableable, collected, rendered, and uploaded; `Renderer` plugins are imported by them and never appear in the configuration.
- `needs_api_key()` makes `setup` ask for a credential and `[plugins.<name>].api_key` be honoured.
- `filename()` defines the file uploaded to the device and the keep list used by `only-stats`.
- `collect()` refreshes the plugin's internal state during each cycle.
- `render()` converts the collected state into the image that will be uploaded.
- `depends_on()` declares other plugins that must also be enabled.

Plugin instances are created once by `plugins::registry()` and reused for the entire process lifetime. Each cycle calls `collect()` and then `render()`. A plugin failure skips only that plugin's screen for the current cycle; other plugins continue.

A plugin keeps its collector next to the renderer it owns. Drawing code shared by several screens becomes a `Renderer` plugin of its own — `agents-usage-ui` draws the usage bars for Claude Code, Codex, and Kimi Code. `src/render/common.rs` contains only genuinely shared primitives such as text, colors, and shapes. Renderer modules remain available at compile time even when their corresponding plugins are disabled in configuration.

### Adding a plugin

1. Create `src/plugins/<name>/mod.rs` and, when needed, `render.rs`.
2. Implement `Plugin` (with `PluginKind::Ui`) and `UiPlugin` for the plugin's main type.
3. Declare the module in `src/plugins/mod.rs`.
4. Add the instance to `ui_plugins()`; the name, kind, and credential lists are derived from it.
5. Configure the plugin:

```toml
[plugins.<name>]
enabled = true
```

If the section or `enabled` is absent, the plugin is enabled by default. Missing or disabled dependencies cause the registry to skip the dependent plugin and print a warning.

## Usage

Run one cycle using the default configuration:

```sh
geekmagic-monitors run
```

Run exactly one cycle, ignoring `interval`:

```sh
geekmagic-monitors run --once
```

Use a different configuration:

```sh
geekmagic-monitors run --config /path/to/config.toml
```

Save screens locally instead of uploading them:

```sh
geekmagic-monitors run --output-dir ./preview --once
```

Force parallel or sequential rendering:

```sh
geekmagic-monitors run --parallel --once
geekmagic-monitors run --no-parallel --once
```

## Daemon

Install and enable the operating system integration (auto-start at login):

```sh
geekmagic-monitors daemon enable
```

Disable and remove it:

```sh
geekmagic-monitors daemon disable
```

Show the service state plus the last cycle's outcome — which plugins succeeded, which failed and why, and the upload result:

```sh
geekmagic-monitors daemon status
```

Restart the background process:

```sh
geekmagic-monitors daemon restart
```

The outcome shown by `daemon status` is written by every run to `~/.config/geekmagic-custom-monitors/status.json`.

## Uninstall

Remove the tool completely:

```sh
geekmagic-monitors uninstall
```

The command disables the automatic startup integration, deletes the tool's root data directory, and removes the running binary itself.

The root data directory is:

```text
~/.config/geekmagic-custom-monitors/
```

It holds the default configuration and every image backup. `daemon disable` never removes it; only `uninstall` does.

## Development and releases

The pinned Rust and Node runtimes are declared in `.vfox.toml`. Install them, then install the Rust standard-library sources used by OMP's `rust-analyzer`:

```sh
vfox install --all
sh scripts/install-rust-src
```

Install the pinned hook manager and all three Git hook types:

```sh
sh scripts/vfox-rust cargo install prek --version 0.5.0 --locked
prek install --hook-type pre-commit --hook-type pre-push --hook-type commit-msg
```

Open OMP at the repository root; `.omp/lsp.json` launches the pinned Rust analyzer through `scripts/vfox-rust`. Hook definitions and CI checks live in `prek.toml` and `.github/workflows/ci-release.yml`.

Use Conventional Commits. `main` publishes stable releases and `develop` publishes `beta` prereleases. `v0.1.0` is the initial baseline. After a stable release, merge `main` back into `develop` before further release-worthy commits.

Each release publishes unsigned CLI archives for `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`, `x86_64-apple-darwin`, and `aarch64-apple-darwin`, plus `SHA256SUMS`. Sync both branches after a release commit before starting subsequent work.
