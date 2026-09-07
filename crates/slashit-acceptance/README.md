# slashit-acceptance

Rust-only desktop acceptance harness. It drives the **real** SlashIt
application — real Tauri process, real Leptos/WASM frontend, real Rust
backend — over the W3C WebDriver protocol, against a private state root.

No Node, npm, pnpm, Bun or WebdriverIO is involved at any point. The client is
[`thirtyfour`]; the Linux provider is the official `tauri-driver`.

## Running the journeys

```bash
# 1. Build the application under test. This step is not optional: a plain
#    `cargo build` produces a binary that loads http://localhost:1420 and
#    shows a connection-error page. `cargo tauri build` enables
#    `custom-protocol`, which embeds dist/ and loads tauri://localhost.
cargo tauri build --debug --no-bundle

# 2. Run them.
cargo test -p slashit-acceptance --features run-acceptance
```

The journeys need a graphical session, `tauri-driver` on `PATH`, and a
`WebKitWebDriver`. The provider is installed with
`cargo install tauri-driver --version "^2" --locked`; on Debian and Ubuntu the
native driver is `apt install webkit2gtk-driver`.

> **Rebuild after any ordinary cargo command that touches `slashit-ui`.**
> `cargo build`, `cargo test -p slashit-ui` and `cargo clippy` all write
> `target/debug/slashit-ui` without `custom-protocol`, silently replacing the
> acceptance binary with one that has no frontend embedded. The two are
> indistinguishable on disk, so the harness detects it at runtime and says so
> rather than timing out. CI is unaffected: the compile job and the acceptance
> job are separate checkouts.

The harness's own unit tests — port reservation, process-group teardown, state
roots, artifact pruning — need none of that and run in the ordinary
`cargo test` sweep.

### Configuration

| Variable | Meaning |
| --- | --- |
| `SLASHIT_ACCEPTANCE_BIN` | The application binary to test. Defaults to `target/debug/slashit-ui`. |
| `SLASHIT_ACCEPTANCE_NATIVE_DRIVER` | Path to `WebKitWebDriver`. Optional; `tauri-driver` searches `PATH` otherwise. |

### Arch Linux

Arch ships no `WebKitWebDriver` binary at all: `webkit2gtk-4.1` installs no
`/usr/bin` entries. Extracting Debian's `webkit2gtk-driver` package for the
same upstream version works. Because it needs its own ICU, point the harness
at a wrapper rather than exporting `LD_LIBRARY_PATH` into the whole test run:

```bash
cat > /tmp/WebKitWebDriver <<'SH'
#!/bin/sh
exec env LD_LIBRARY_PATH=/path/to/icu/usr/lib/x86_64-linux-gnu \
    /path/to/deb/root/usr/bin/WebKitWebDriver "$@"
SH
chmod +x /tmp/WebKitWebDriver
export SLASHIT_ACCEPTANCE_NATIVE_DRIVER=/tmp/WebKitWebDriver
```

## What the harness guarantees

- **Isolation.** Every run gets a fresh `XDG_CONFIG_HOME`, `XDG_DATA_HOME`,
  `XDG_CACHE_HOME` and `XDG_RUNTIME_DIR` under a temporary directory. The
  developer's real SlashIt state is never read or written.
- **Dynamic ports.** No fixed 4444/4445. Ports come from the kernel, are held
  until the moment the provider is spawned, and are re-checked afterwards.
- **Ownership.** The provider runs as the leader of its own process group, and
  teardown signals the group — `tauri-driver` does not reap `WebKitWebDriver`,
  and `WebKitWebDriver` does not reap the application.
- **Verified cleanup.** After each session the harness proves no process is
  still running against the run's state root, by matching `/proc/*/environ`
  rather than trusting a process name or a signal's return value.
- **Real readiness.** Waits are on conditions — the provider accepting
  connections, the session existing, the Leptos root mounting — never on a
  fixed sleep.
- **Evidence on failure.** URL, title, screenshot, page source and the
  provider log are captured while the application is still alive, into
  `target/acceptance/`. Successful runs delete everything; failed runs are
  retained, oldest first, up to a bounded count.

## Platforms

**The acceptance runtime is implemented for Linux only.** `ports`,
`diagnostics` and `ui` speak nothing but the WebDriver protocol and compile
everywhere; `driver`, `process`, `state` and `context` are
`#[cfg(target_os = "linux")]`, because behind them are Unix process groups,
POSIX signals, `/proc` and XDG directories. `tests/acceptance.rs` is gated the
same way.

macOS and Windows are therefore *unimplemented*, not merely untested. Each
would add its own `#[cfg]` implementation of the same module names — a macOS
provider driving the embedded `tauri-plugin-wdio-webdriver` server, a Windows
one driving `msedgedriver` with Job Objects instead of process groups — and
reuse the portable half unchanged. Until then, CI compiles this crate on
macOS and Windows so the boundary fails loudly if a `/proc` read or a
`libc::kill` escapes its gate.

## Scope

Two journeys, `app_boots_and_frontend_is_real` and `state_survives_restart`.
They exist to prove the harness, not the product. Broader acceptance coverage
belongs in follow-up suites built on these primitives.

[`thirtyfour`]: https://docs.rs/thirtyfour
