// SPDX-License-Identifier: MPL-2.0
// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(target_os = "linux")]
mod rtprio_guard;

use clap::Parser;

/// Command line of the desktop binary. It takes no options: its only job is to
/// answer `--version`, `--help`, and reject anything else before any state is
/// created. `slashitd` is the headless entry point with options.
#[derive(Parser)]
#[command(
    name = "slashit-ui",
    about = "SlashIt desktop app. Run without arguments to open the window.",
    version
)]
struct Cli {}

/// Parses the arguments without starting anything. `Ok` means the GUI should
/// start. `Err` carries the clap outcome: help or version (exit 0), or a usage
/// error (exit 2).
fn parse_arguments<I, T>(args: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    Cli::try_parse_from(args)
}

fn main() {
    // Arguments come first. Anything but a plain launch exits here, before the
    // rtprio guard, the IPC claim, or any path, config, or storage is touched.
    if let Err(error) = parse_arguments(std::env::args_os()) {
        // Prints to stdout for --help/--version and to stderr for usage errors.
        error.exit();
    }

    // This must run before the first window, because WebKit makes its
    // scheduling request while creating it. The guard applies only to the
    // WebKitGTK versions that need it. See `rtprio_guard`.
    #[cfg(target_os = "linux")]
    if let Err(error) = rtprio_guard::apply_startup_guard() {
        // Startup continues, since the app still works without the guard. The
        // message says the protection is missing, so it is not silently assumed.
        eprintln!(
            "SlashIt could not lower the realtime priority limit; the WebKit startup guard is not active: {error}"
        );
    }

    slashit_ui_lib::run()
}

#[cfg(test)]
mod tests {
    use super::parse_arguments;
    use clap::error::ErrorKind;

    #[test]
    fn no_arguments_starts_the_gui() {
        assert!(parse_arguments(["slashit-ui"]).is_ok());
    }

    #[test]
    fn version_flags_return_a_non_start_outcome() {
        for flag in ["--version", "-V"] {
            let error = parse_arguments(["slashit-ui", flag])
                .err()
                .unwrap_or_else(|| panic!("{flag} must not start the GUI"));
            assert_eq!(error.kind(), ErrorKind::DisplayVersion, "{flag}");
            assert_eq!(error.exit_code(), 0, "{flag}");
        }
    }

    #[test]
    fn help_flags_return_a_non_start_outcome() {
        for flag in ["--help", "-h"] {
            let error = parse_arguments(["slashit-ui", flag])
                .err()
                .unwrap_or_else(|| panic!("{flag} must not start the GUI"));
            assert_eq!(error.kind(), ErrorKind::DisplayHelp, "{flag}");
            assert_eq!(error.exit_code(), 0, "{flag}");
        }
    }

    #[test]
    fn unknown_arguments_are_usage_errors() {
        for args in [
            vec!["slashit-ui", "--bogus"],
            vec!["slashit-ui", "extra"],
            vec!["slashit-ui", "--verbose"],
        ] {
            let error = parse_arguments(args.clone())
                .err()
                .unwrap_or_else(|| panic!("{args:?} must not start the GUI"));
            assert_eq!(error.exit_code(), 2, "{args:?}");
        }
    }
}
