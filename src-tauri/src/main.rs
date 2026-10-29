// SPDX-License-Identifier: MPL-2.0
// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(target_os = "linux")]
mod rtprio_guard;

fn main() {
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
