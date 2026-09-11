# Contributing to metaText

Thanks for your interest in improving metaText! This document explains how to
set up the project, the conventions we follow, and the process for getting a
change merged.

## Code of conduct

By participating in this project you agree to abide by our
[Code of Conduct](CODE_OF_CONDUCT.md). Please report unacceptable behaviour to
[arksong2018@gmail.com](mailto:arksong2018@gmail.com).

## Prerequisites

- **Rust** — install via [rustup](https://rustup.rs/). The pinned toolchain is
  declared in [`rust-toolchain.toml`](rust-toolchain.toml) and is applied
  automatically.
- **A C toolchain** (`cc`) — required transitively by some Linux crates.
- **SQLite** — only needed at runtime for the optional `sqlite` feature; the
  bundled `libsqlite3-sys` compiles it for you.

## Getting started

```bash
git clone https://github.com/arkCyber/metaText.git
cd metaText

# Format, lint and test everything the way CI does
cargo fmt --all -- --check
cargo clippy --all-targets --all-features
cargo test --all-features
```

Build and run the default (light) profile:

```bash
cargo run
```

…and the full feature set:

```bash
cargo run --features sqlite,terminal-ui -- --mode cli
```

## Coding standards

- **Language** — code, comments, commit messages and documentation are written
  in English.
- **Formatting** — run `cargo fmt` before committing. The configuration lives in
  [`rustfmt.toml`](rustfmt.toml) (max width 100, 4 spaces).
- **Linting** — the crate enables `clippy::all`, `clippy::pedantic`,
  `clippy::nursery` and `clippy::cargo` as warnings. Fix new warnings you
  introduce.
- **Documentation** — every public item must be documented; the crate uses
  `#![deny(missing_docs)]`. Include runnable `/// # Examples` where it helps.
- **Unsafe** — `unsafe` code is forbidden (`#![deny(unsafe_code)]`); use safe
  abstractions such as `tokio` and `chacha20poly1305`.
- **Errors** — return `Result` with `anyhow`/`thiserror`; never `unwrap()` on
  user input or I/O in library code.

## Testing

- Unit tests live next to the code in `#[cfg(test)]` modules.
- Integration tests live in [`tests/`](tests) and drive the public API or the
  compiled binary.
- Add a test for every bug fix and every new behaviour.
- Keep tests hermetic: bind to port `0`, use `tempfile` for on-disk state, and
  never depend on the network.

```bash
cargo test                       # default features
cargo test --all-features        # everything, the CI configuration
```

## Commit messages

We follow [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<scope>): <short summary>

<optional body>

<optional footer>
```

Common types: `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`,
`build`, `ci`, `chore`, `revert`. Keep the summary in the imperative mood and
under 72 characters.

## Pull requests

1. Fork the repository and create a topic branch off `main`:
   `git checkout -b feat/my-change`.
2. Make your change, add tests and documentation, and keep the diff focused.
3. Ensure the full check suite passes locally:
   `cargo fmt --all -- --check && cargo clippy --all-targets --all-features && cargo test --all-features`.
4. Update [`CHANGELOG.md`](CHANGELOG.md) under the `Unreleased` heading when the
   change is user-visible.
5. Push the branch, open a pull request, and fill in the template. CI must be
   green before review.

Small, well scoped pull requests are reviewed fastest. If you plan a large
change, open an issue first so we can agree on the approach.

## Release process (maintainers)

1. Move the `Unreleased` entries in `CHANGELOG.md` into a new version heading.
2. Bump `version` in [`Cargo.toml`](Cargo.toml).
3. Commit as `chore(release): vX.Y.Z` and tag it: `git tag -a vX.Y.Z -m "vX.Y.Z"`.
4. Push the commit and tag; the release workflow publishes the binaries.
