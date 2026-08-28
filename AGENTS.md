## Rust toolchain

Use `rustc`, `cargo`, `rustfmt`, `clippy`, and `rust-analyzer` resolved from `PATH` first. Do not create wrappers or add SDK or version-manager paths to project files. If a required Rust tool is unavailable, check `vfox`; when it already has a Rust SDK installed, activate it for the session with `vfox use --session rust@<version>` and continue using normal Rust commands. If that session activation cannot be inherited by the process, run only the required command with `vfox exec rust@<version> -- <command>` or `vfox x rust@<version> -- <command>`. Obtain `<version>` from the toolchain version in `.github/workflows/ci-release.yml`; do not duplicate it here.

## Commits and releases

Every commit requested of an agent must use Conventional Commits in the literal format `<type>[optional scope][!]: <description>`. `feat`, `fix`, `perf`, and `revert` generate releases; `!` or a `BREAKING CHANGE` footer generates a major release; `docs`, `chore`, `ci`, `test`, `refactor`, `style`, and `build` generate no release when there is no breaking change. `semantic-release` depends on this classification to calculate semantic versions.
