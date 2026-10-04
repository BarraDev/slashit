# slashit-acceptance

Rust-only desktop acceptance harness. It drives the **real** SlashIt
application — real Tauri process, real Leptos/WASM frontend, real Rust
backend — over the W3C WebDriver protocol, against a private state root.

No Node, npm, pnpm, Bun or WebdriverIO is involved at any point. The client is
[`thirtyfour`]; the Linux provider is the official `tauri-driver`.

## Running the journeys

```bash
# 1. Build the application under test. CI runs the same script.
scripts/build-acceptance-app.sh

# 2. Run them: every product journey, one exact journey, one journey on your
#    own display, one CI shard, or the harness suite.
scripts/run-desktop-acceptance.sh
scripts/run-desktop-acceptance.sh --exact storage::storage_settings_measure_disk_usage_and_offer_nothing_destructive
scripts/run-desktop-acceptance.sh --visible --exact <name>
scripts/run-desktop-acceptance.sh --shard 1
scripts/run-desktop-acceptance.sh --harness
```

`--list` prints the selected test names, and arguments after `--` go to the
test binary unchanged. Journeys always run one at a time.

**CI shards.** Hosted CI splits the product journeys across three runners
(`Desktop acceptance / product shard 1..3`) so they run concurrently instead
of one 11-12 minute job; a separate `Desktop acceptance / harness` job runs
`tests/acceptance.rs` once and is not sharded. `--shard <N>` reads
[`shard-manifest.txt`](shard-manifest.txt) and runs exactly the journeys CI's
shard `N` runs, so a shard failure reproduces locally without retyping test
names. `scripts/check-acceptance-shards.sh` (run in the "Lint and
structural checks" job) proves the manifest assigns every journey `cargo test --list`
reports to exactly one shard; editing the manifest to rebalance shards or add
a newly-written journey to one is normal, but leaving a journey unassigned or
in two shards fails that check.

The build step is not optional: a plain `cargo build` produces a binary that
loads http://localhost:1420 and shows a connection-error page. The script runs
`cargo tauri build --debug --no-bundle`, which enables `custom-protocol`,
embeds dist/ and loads tauri://localhost. Before embedding, it removes the
frontend module's WebAssembly `name` section. In a debug build that section
is about nine tenths of the module, and it holds function names for stack
traces, which the harness never collects. Keeping it costs seconds on every
application start. The removal is verified: every other section, code and
data included, must be byte-identical, or the build fails
(`src/wasm_names.rs`). The Subresource Integrity pin Trunk writes for the
module in `dist/index.html` moves to the stripped module, and nothing else in
the page changes. The profile, debug assertions and `trunk serve` are
unchanged. The script records the binary it built, and the runner refuses a
binary that has changed since.

**Display.** By default the runner starts a private Xvfb display and a private
D-Bus session for the run and points only the test process tree at them
(`GDK_BACKEND=x11`, no `WAYLAND_DISPLAY`). The private bus activates no
services, so none of your desktop's services start with your real HOME. No window opens on your desktop,
and the application's tray icon does not reach your panel. `--visible` uses
your own session instead. Hidden mode never falls back to it: without Xvfb it
stops and says what to install (Arch `xorg-server-xvfb`, Debian and Ubuntu
`xvfb`).

The journeys also need `tauri-driver` on `PATH` and a `WebKitWebDriver`. The
provider is installed with `cargo install tauri-driver --version "^2" --locked`;
on Debian and Ubuntu the native driver is `apt install webkit2gtk-driver`.

> **Rebuild after any ordinary cargo command that touches `slashit-ui`.**
> `cargo build`, `cargo test -p slashit-ui` and `cargo clippy` all write
> `target/debug/slashit-ui` without `custom-protocol`, silently replacing the
> acceptance binary with one that has no frontend embedded. The runner notices
> the changed binary before starting, and the harness also detects a
> development build at runtime rather than timing out. CI is unaffected: the
> compile job and the acceptance job are separate checkouts.

The harness's own unit tests — port reservation, process-group teardown, state
roots, artifact pruning — need none of that and run in the ordinary
`cargo test` sweep.

### Configuration

| Variable | Meaning |
| --- | --- |
| `SLASHIT_ACCEPTANCE_BIN` | The application binary to test. Defaults to `target/debug/slashit-ui`. |
| `SLASHIT_ACCEPTANCE_NATIVE_DRIVER` | Path to `WebKitWebDriver`. Optional; `tauri-driver` searches `PATH` otherwise. |

The harness also sets `SLASHIT_DEBUG_DISK_SPACE_FILE` for the application
itself; see Isolation below.

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
  The application's free disk space is faked too: every run starts with a
  file reporting plenty of space, which debug builds of SlashIt read through
  `SLASHIT_DEBUG_DISK_SPACE_FILE` instead of the real filesystem when they
  decide whether to start a task. On a host with little free space, a
  release binary ignores it and pauses every start, so test a debug build.
- **No developer-installed CodeRabbit.** The application never finds a
  `coderabbit` CLI in any directory named on its `PATH`, so AI Review takes
  the same "CLI not found" path locally that it takes in CI, and no journey
  calls the CodeRabbit service or touches your credentials for it. Every
  `PATH` entry that holds `coderabbit` is replaced, for the WebDriver
  provider and the application it launches, by a mirror of that directory
  without it, and empty or relative entries are dropped
  (`src/developer_tools.rs`).
  This is the only tool isolated this way, and it is removed only by that
  name. The rest of `PATH` is still yours, not hermetic: `git`, `jj` and
  everything else resolve as they do in your shell, except `claude` and
  `gh`, which the journeys that use them shadow with fixtures. Your shell and
  your installation are not changed.
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
- **A click that opens nothing says why.** `open_drawer` clicks a card once.
  If its drawer does not appear, the failure message carries what the page
  saw: each pointer, mouse, drag and scroll event with where the card was,
  which task the card showed, whether the card or the element the button went
  down on left the page, any drawer that appeared or went, the board's scroll
  position and the element at the card's centre. A click whose button comes
  up over a different element than it went down on is delivered to their
  common ancestor. When that ancestor is not the card, the card's handler
  never sees the click, so a card that moves between the two events can open
  nothing. The event targets can help tell that apart from other failures;
  they do not identify the cause. The probe listens before the card's own
  handler runs, so the record does not show whether that handler ran.

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
