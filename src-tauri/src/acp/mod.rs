// SPDX-License-Identifier: MPL-2.0
pub mod protocol;
pub mod client;

pub use client::*;

// The ACP v1 client: the OpenCode Worker runs through it. See `agents::opencode_run`.
pub mod v1;
