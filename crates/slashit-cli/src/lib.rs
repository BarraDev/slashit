// SPDX-License-Identifier: MPL-2.0
//! The `slashit` command line client, as a library as well as a binary.
//!
//! The library form exists so the backend's own tests can drive the binary's
//! pieces — most importantly [`output`], the formatting the `slashit runs`
//! report is printed through — against answers the backend's handlers
//! actually produce, rather than against reconstructed samples.
pub mod client;
pub mod output;
