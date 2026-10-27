// SPDX-License-Identifier: MPL-2.0
// The channel parser is shared with the application, so the build and the app
// read the declaration with one implementation. See src/release_channel.rs.
#[path = "src/release_channel.rs"]
mod release_channel;

use release_channel::{channel_for, parse_declarations, PlainVersion};
use std::path::PathBuf;

fn main() {
    tauri_build::build();

    // The build's own channel. A version the declaration does not name fails
    // the build, so no binary ever reports a channel it was not declared as.
    let version_text = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let path = PathBuf::from(manifest_dir).join("../.github/release-channels.txt");
    println!("cargo:rerun-if-changed={}", path.display());

    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    let declarations =
        parse_declarations(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let version = PlainVersion::parse(&version_text)
        .unwrap_or_else(|| panic!("the package version {version_text} is not plain MAJOR.MINOR.PATCH"));
    let channel = channel_for(&declarations, version).unwrap_or_else(|| {
        panic!("{version} has no channel in {}; declare it there", path.display())
    });
    println!("cargo:rustc-env=SLASHIT_RELEASE_CHANNEL={}", channel.as_str());
}
