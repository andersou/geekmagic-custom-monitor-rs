# geekmagic-custom-monitor-rs

Extensible Rust monitors for GeekMagic SmallTV displays. The program collects metrics, renders images, and uploads them to the device in album mode.

Based on [geekmagic-stats](https://github.com/jimmystridh/geekmagic-stats).

## Features

- Built-in plugins for Claude Code usage and local disk usage.
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

[plugins.claude]
enabled = true

[plugins.disk]
enabled = true
```

Backups created by `only-stats` are stored in:

```text
~/.config/geekmagic-custom-monitors/backups/
```

A device file is deleted only after its local backup has been saved successfully. Cleanup and backup happen once during process startup; later update cycles only render and upload screens. If there is no file that can be saved, no empty backup directory remains or counts toward retention. `backup_retention` must be at least `1`.

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
+------------------------------------------+
|                                          |
|       src/plugin.rs - Plugin trait       |
|                                          |
+------------------------------------------+
                      v
+------------------------------------------+
|                                          |
|      src/plugins/mod.rs - registry       |
|                                          |
+------------------------------------------+
                      v---------------------------------------------v
+------------------------------------------+      +----------------------------------+
|                                          |      |                                  |
|    claude/mod.rs - collect and Plugin    |      | disk/mod.rs - collect and Plugin |
|                                          |      |                                  |
+------------------------------------------+      +----------------------------------+
                      v                                             v
+------------------------------------------+      +----------------------------------+
|                                          |      |                                  |
|   claude/render.rs - render claude.jpg   |      | disk/render.rs - render disk.jpg |
|                                          |      |                                  |
+-------------------uses-------------------+      +----------------------------------+
                      v                                             |
+------------------------------------------+                        |
|                                          |                        |
| src/render/common.rs - shared primitives |<--------uses-----------+
|                                          |
+------------------------------------------+
```

The contract is defined in `src/plugin.rs`:

```rust
pub trait Plugin: Send {
    fn name(&self) -> &'static str;
    fn filename(&self) -> &'static str;
    fn collect(&mut self) -> anyhow::Result<()>;
    fn render(&self) -> anyhow::Result<image::RgbaImage>;
    fn depends_on(&self) -> &'static [&'static str] {
        &[]
    }
}
```

- `name()` is the key used by `[plugins.<name>]` in TOML.
- `filename()` defines the file uploaded to the device and the keep list used by `only-stats`.
- `collect()` refreshes the plugin's internal state during each cycle.
- `render()` converts the collected state into the image that will be uploaded.
- `depends_on()` declares other plugins that must also be enabled.

Plugin instances are created once by `plugins::registry()` and reused for the entire process lifetime. Each cycle calls `collect()` and then `render()`. A plugin failure skips only that plugin's screen for the current cycle; other plugins continue.

Each plugin keeps its collector and renderer in its own directory. `src/render/common.rs` contains only genuinely shared primitives such as text, colors, and shapes. Renderer modules remain available at compile time even when their corresponding plugins are disabled in configuration.

### Adding a plugin

1. Create `src/plugins/<name>/mod.rs` and, when needed, `render.rs`.
2. Implement `Plugin` for the plugin's main type.
3. Declare the module in `src/plugins/mod.rs`.
4. Add the name to `known_plugin_names()` and the instance to `registry()`.
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

## Automatic startup

Install and enable the operating system integration:

```sh
geekmagic-monitors boot enable
```

Disable and remove it:

```sh
geekmagic-monitors boot disable
```

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

It holds the default configuration and every image backup. `boot disable` never removes it; only `uninstall` does.
