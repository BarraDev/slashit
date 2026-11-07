// SPDX-License-Identifier: MPL-2.0
//! Portable Project export (version 1) and read-only import preview.
//!
//! - [`bundle`]: the versioned file format, checksum and structural validation.
//! - [`export`]: the refusal rules, the portable projection, and the atomic
//!   no-clobber write.
//! - [`preview`]: validation of a bundle against the target's in-memory state.
//! - [`apply`]: creates one inert Project and its Tasks from a confirmed bundle.
//!
//! Exposed through IPC (`ExportProject`, `PreviewImport`, `ApplyImport`). An
//! apply creates no Repository, and nothing it creates can run without the
//! usual setup and admission.

pub mod apply;
pub mod bundle;
pub mod export;
pub mod live;
pub mod preview;

#[cfg(test)]
mod apply_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod verification;
