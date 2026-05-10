# Contributing to SlashIt

Thanks for your interest in contributing! This document explains how to set up your environment, propose changes, and submit pull requests.

## Code of Conduct

This project follows the [Contributor Covenant Code of Conduct](CODE_OF_CONDUCT.md). By participating, you agree to uphold its terms.

## Reporting Issues

Before opening an issue, please:

1. Search existing [issues](https://github.com/BarraDev/slashit/issues) to avoid duplicates
2. Use the appropriate issue template (bug report or feature request)
3. Include reproduction steps, expected vs actual behavior, and environment details

For **security vulnerabilities**, do not open a public issue. See [SECURITY.md](SECURITY.md).

## Development Setup

### Prerequisites

- Rust (stable toolchain) -- install via [rustup](https://rustup.rs/)
- [Trunk](https://trunkrs.dev/) -- `cargo install trunk`
- `wasm32-unknown-unknown` target -- `rustup target add wasm32-unknown-unknown`
- Tauri CLI -- `cargo install tauri-cli --version "^2.0"`
- Linux: `libwebkit2gtk-4.1-dev`, `libgtk-3-dev`, `libayatana-appindicator3-dev`, `librsvg2-dev`

### Build & Run

```bash
# Clone and enter the repo
git clone https://github.com/BarraDev/slashit.git
cd slashit

# Run the desktop app in dev mode
./dev.sh

# Or just the frontend
trunk serve

# Build the CLI
cargo build -p slashit --release
```

## Code Style

- **Formatting:** `cargo fmt` (run before committing)
- **Linting:** `cargo clippy --all-targets --all-features -- -D warnings`
- **Tests:** `cargo test`

PRs that fail CI will not be merged. Run the checks locally first.

## Pull Request Process

1. **Fork** the repository
2. **Branch** from `main`: `git checkout -b feat/my-feature`
3. **Commit** with a clear message describing the change (see commit conventions below)
4. **Test** locally: `cargo test && cargo clippy && trunk build`
5. **Push** and open a Pull Request against `main`
6. **Fill in** the PR template -- describe what changed and how to test
7. **Address** review feedback by pushing additional commits

### Commit Message Convention

We loosely follow [Conventional Commits](https://www.conventionalcommits.org/):

- `feat:` new feature
- `fix:` bug fix
- `refactor:` code restructure without behavior change
- `test:` adding or improving tests
- `docs:` documentation only
- `chore:` build, tooling, dependencies

Example: `feat: Add task drag-and-drop between Kanban columns`

## Project Structure

See [AGENTS.md](AGENTS.md) for an architecture overview and module-by-module guide.

## Version Control: Jujutsu (jj)

This project uses [Jujutsu](https://github.com/martinvonz/jj) as its primary VCS, colocated with Git for compatibility. Contributors using plain Git are welcome -- the Git interop is seamless.

If you'd like to use `jj`, see [the Jujutsu docs](https://jj-vcs.github.io/jj/latest/).

## License

By contributing, you agree that your contributions will be licensed under the [Apache License 2.0](LICENSE), the same license as the project.
