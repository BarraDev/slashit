#!/usr/bin/env bash
# Run the desktop acceptance journeys against the application built by
# scripts/build-acceptance-app.sh.
#
# By default the application runs on a private Xvfb display and a private
# D-Bus session started for this run, so no window opens, takes focus or
# switches workspace on your real desktop, and no tray icon appears in your
# panel. --visible runs it on your own graphical session instead, for
# watching one journey. There is no fallback between the two: without Xvfb,
# hidden mode fails rather than quietly opening windows.
#
# Journeys always run one at a time.
set -euo pipefail

usage() {
  cat <<'EOF'
usage: scripts/run-desktop-acceptance.sh [options] [-- <libtest args>]

  (no options)      every product journey, on a private display
  --exact <name>    only the journey with this exact test name
  --harness         the harness suite (tests/acceptance.rs) instead
  --visible         use your real display instead of a private one
  --list            list the selected tests without running them

Arguments after -- go to the test binary unchanged. --test-threads is fixed
at 1. Build the application first with scripts/build-acceptance-app.sh.
EOF
}

fail() {
  echo "run-desktop-acceptance: $*" >&2
  exit 1
}

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

visible=0
list=0
target=product_acceptance
filter=
libtest=()
while (($#)); do
  case $1 in
    --visible) visible=1 ;;
    --harness) target=acceptance ;;
    --list) list=1 ;;
    --exact)
      (($# >= 2)) || fail "--exact needs a test name"
      filter=$2
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    --)
      shift
      libtest+=("$@")
      break
      ;;
    *) fail "unknown argument: $1 (see --help)" ;;
  esac
  shift
done
for arg in ${libtest[@]+"${libtest[@]}"}; do
  [[ $arg == --test-threads* ]] && fail "journeys run one at a time; --test-threads is fixed at 1"
done

test_args=(--test-threads=1)
[[ -n $filter ]] && test_args+=(--exact "$filter")
((list)) && test_args+=(--list)
test_args+=(${libtest[@]+"${libtest[@]}"})
cargo_test=(cargo test -p slashit-acceptance --features run-acceptance --test "$target" -- "${test_args[@]}")

echo "target:  $target${filter:+ (--exact $filter)}"

# Listing launches nothing, so it needs neither a display nor a build.
if ((list)); then
  exec "${cargo_test[@]}"
fi

# libtest reports success when a filter matches nothing, so a mistyped or
# renamed journey would pass without running. Refuse that up front.
if [[ -n $filter ]]; then
  listed=$(cargo test -q -p slashit-acceptance --features run-acceptance --test "$target" -- --list --exact "$filter")
  [[ $listed == *": test"* ]] || fail "no test in $target is named exactly '$filter' (see --list)"
fi

# The application under test. scripts/build-acceptance-app.sh records the
# binary it built; any ordinary cargo build of slashit-ui silently replaces
# that file with one that embeds no frontend or an unstripped one.
stamp=target/acceptance-app.stamp
if [[ -n ${SLASHIT_ACCEPTANCE_BIN:-} ]]; then
  echo "app:     $SLASHIT_ACCEPTANCE_BIN (from SLASHIT_ACCEPTANCE_BIN; not checked by this script)"
else
  [[ -f $stamp ]] || fail "no acceptance build found; run scripts/build-acceptance-app.sh first"
  binary=$(sed -n 's/^binary=//p' "$stamp")
  expected=$(sed -n 's/^sha256=//p' "$stamp")
  wasm_bytes=$(sed -n 's/^wasm_bytes=//p' "$stamp")
  [[ -f $binary ]] || fail "$binary is missing; run scripts/build-acceptance-app.sh"
  actual=$(sha256sum "$binary" | cut -d' ' -f1)
  [[ $actual == "$expected" ]] ||
    fail "$binary was rebuilt since scripts/build-acceptance-app.sh made it (cargo build, clippy, test or tauri build replace it); run scripts/build-acceptance-app.sh again"
  echo "app:     $binary (acceptance build, frontend module $wasm_bytes bytes)"
fi

if ((visible)); then
  echo "display: visible, on your session (DISPLAY=${DISPLAY:-unset}, WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-unset})"
  exec "${cargo_test[@]}"
fi

command -v Xvfb >/dev/null ||
  fail "Xvfb is not installed, and hidden mode will not fall back to your real display.
  Arch:          sudo pacman -S xorg-server-xvfb
  Debian/Ubuntu: sudo apt install xvfb
Or pass --visible to run on your own display deliberately."
command -v dbus-run-session >/dev/null ||
  fail "dbus-run-session is not installed (package dbus on Arch, dbus-bin or dbus on Debian/Ubuntu).
Hidden mode needs it to keep the application off your real session bus."

scratch=$(mktemp -d "${TMPDIR:-/tmp}/slashit-acceptance-display.XXXXXX")
# The path goes unescaped into a D-Bus address and its XML configuration, and
# a Unix socket path is limited to 108 bytes.
if [[ ! $scratch =~ ^[A-Za-z0-9/._-]+$ ]] || ((${#scratch} > 80)); then
  rm -rf "$scratch"
  fail "the scratch directory $scratch is unusable for a socket; set TMPDIR to a short, plain path"
fi
xvfb_pid=
cleanup() {
  local status=$?
  if ((status != 0)) && [[ -s $scratch/xvfb.log ]]; then
    echo "run-desktop-acceptance: Xvfb log:" >&2
    cat "$scratch/xvfb.log" >&2
  fi
  if [[ -n $xvfb_pid ]]; then
    kill "$xvfb_pid" 2>/dev/null || true
    wait "$xvfb_pid" 2>/dev/null || true
  fi
  rm -rf "$scratch"
}
trap cleanup EXIT

# -displayfd: the server picks a free display number and reports it once it
# accepts connections, so there is neither a number to collide on nor a
# readiness sleep.
Xvfb -displayfd 3 -screen 0 1280x1024x24 -nolisten tcp 3>"$scratch/display" 2>"$scratch/xvfb.log" &
xvfb_pid=$!
for _ in $(seq 100); do
  grep -q '^[0-9]' "$scratch/display" 2>/dev/null && break
  kill -0 "$xvfb_pid" 2>/dev/null || fail "Xvfb exited during startup"
  sleep 0.1
done
grep -q '^[0-9]' "$scratch/display" || fail "Xvfb reported no display within 10s"
display=":$(head -n1 "$scratch/display")"

echo "display: hidden, private Xvfb $display"

# A session bus with no service directories: the application's own calls
# (the tray icon registering) go nowhere, and nothing can activate the
# developer's desktop services -- dconf, the keyring, portals -- which would
# run with the real HOME rather than the harness's private state.
cat >"$scratch/session.conf" <<EOF
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir=$scratch</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
EOF

echo "bus:     private D-Bus session for this run, no service activation"

# Only this process tree sees these. GDK_BACKEND=x11 pins GTK and WebKit to
# the private X display even though a Wayland session is running, and the
# Wayland variables are removed so nothing can reach the real compositor.
# The display is not the only way onto the desktop: the application's tray
# icon registers over the D-Bus session bus, and on the real bus it shows up
# in the real panel. dbus-run-session gives the run a bus of its own and
# stops it afterwards.
env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET \
  DISPLAY="$display" GDK_BACKEND=x11 \
  dbus-run-session --config-file="$scratch/session.conf" -- "${cargo_test[@]}"
