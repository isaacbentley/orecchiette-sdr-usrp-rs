# Contributing to orecchiette-sdr-usrp-rs

First off, thank you for considering contributing! This crate
implements the `SdrSource` trait (from `orecchiette-sdr-source-rs`)
for Ettus USRP devices via the `uhd` crate.

## Quick Start

```bash
git clone https://github.com/isaacbentley/orecchiette-sdr-usrp-rs.git
cd orecchiette-sdr-usrp-rs

# Requires libuhd-dev (Ubuntu/Debian) or the UHD driver package for
# your platform to build at all.
cargo test
cargo clippy --all-features --all-targets -- -D warnings
cargo fmt --all --check
cargo deny check
```

## Testing Hardware Changes

Most of this crate's logic (master-clock selection, decimation,
dwell/hop pacing) can be unit-tested without a device attached. If
your change affects the actual capture loop, please test against
real B210/B205mini hardware before opening a PR and note which
device you tested with.

## Code Style

We use standard `rustfmt` defaults. Please run `cargo fmt --all` before pushing.

Clippy is run with `-D warnings` in CI. If a lint is genuinely wrong for the situation, allow it with a `// ALLOW:` justification comment explaining why.

## Pull Requests

- **Commit messages:** Describe *why* the change is needed and *what* it changes.
- **Templates:** Please fill out the Pull Request template when opening a PR.

## License

By contributing, you agree your contributions will be licensed under GPL-3.0-or-later, the same as the rest of the project.
