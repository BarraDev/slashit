//! Removing the WebAssembly `name` section from the acceptance build, and
//! proving that nothing else changed.
//!
//! A debug `trunk build` emits a frontend module that is roughly nine tenths
//! function names: the `name` custom section, which exists only so a stack
//! trace can say which function it is in. The acceptance harness never
//! collects a stack trace, but WebKit still loads, hashes and parses the whole
//! module in every session, which costs seconds per application start.
//!
//! Trunk also pins the module in `index.html` with a Subresource Integrity
//! hash on its preload link. [`replace_integrity`] moves that pin to the
//! stripped module, so the page under test does not carry a stale pin that
//! the product's page never has.
//!
//! Stripping it must not change what is being tested, so this module is
//! deliberately not an optimiser. It walks the section headers and copies
//! every section except `name` byte for byte. [`verify`] then re-reads both
//! modules independently and refuses the result unless the stripped module is
//! the original with exactly its `name` section missing: same sections, same
//! order, same bytes. The code and data sections are therefore identical by
//! construction and by check, and a future change that touched anything else
//! -- a different tool, an "improvement" here -- fails the build instead of
//! quietly testing a different program.

use std::ops::Range;

use anyhow::{bail, ensure, Context, Result};
use base64::Engine as _;
use sha2::{Digest, Sha384};

const MAGIC: &[u8; 4] = b"\0asm";
const VERSION: &[u8; 4] = &[1, 0, 0, 0];
const HEADER_LEN: usize = 8;

/// The one custom section this module is allowed to remove.
pub const NAME_SECTION: &str = "name";

const CUSTOM: u8 = 0;
const CODE: u8 = 10;
const DATA: u8 = 11;

/// One top-level section of a module, located by byte range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// The section id. `0` is a custom section.
    pub id: u8,
    /// A custom section's name; `None` for every standard section.
    pub custom_name: Option<String>,
    /// The whole section, id byte and size included.
    pub range: Range<usize>,
}

impl Section {
    fn is_name(&self) -> bool {
        self.custom_name.as_deref() == Some(NAME_SECTION)
    }

    fn len(&self) -> usize {
        self.range.len()
    }

    /// A human label: the custom section's name, or the standard section's.
    pub fn label(&self) -> String {
        match &self.custom_name {
            Some(name) => format!("custom:{name}"),
            None => match self.id {
                1 => "type".into(),
                2 => "import".into(),
                3 => "function".into(),
                4 => "table".into(),
                5 => "memory".into(),
                6 => "global".into(),
                7 => "export".into(),
                8 => "start".into(),
                9 => "element".into(),
                CODE => "code".into(),
                DATA => "data".into(),
                12 => "datacount".into(),
                13 => "tag".into(),
                id => format!("id{id}"),
            },
        }
    }
}

/// Split a module into its sections. Only the header layout is read; section
/// contents are never interpreted beyond a custom section's name.
pub fn sections(module: &[u8]) -> Result<Vec<Section>> {
    ensure!(
        module.len() >= HEADER_LEN && &module[..4] == MAGIC,
        "not a WebAssembly module (bad magic)"
    );
    ensure!(
        &module[4..HEADER_LEN] == VERSION,
        "unsupported WebAssembly version {:?}",
        &module[4..HEADER_LEN]
    );

    let mut sections = Vec::new();
    let mut at = HEADER_LEN;
    while at < module.len() {
        let start = at;
        let id = module[at];
        at += 1;
        let size = read_u32(module, &mut at)
            .with_context(|| format!("section at byte {start}: unreadable size"))?
            as usize;
        let end = at
            .checked_add(size)
            .filter(|&end| end <= module.len())
            .with_context(|| format!("section at byte {start} runs past the end of the module"))?;

        let custom_name = if id == CUSTOM {
            let mut cursor = at;
            let len = read_u32(module, &mut cursor)
                .with_context(|| format!("custom section at byte {start}: unreadable name"))?
                as usize;
            let name_end = cursor
                .checked_add(len)
                .filter(|&name_end| name_end <= end)
                .with_context(|| format!("custom section at byte {start}: name overruns it"))?;
            let name = std::str::from_utf8(&module[cursor..name_end])
                .with_context(|| format!("custom section at byte {start}: name is not UTF-8"))?;
            Some(name.to_owned())
        } else {
            None
        };

        sections.push(Section {
            id,
            custom_name,
            range: start..end,
        });
        at = end;
    }
    Ok(sections)
}

/// Unsigned LEB128, at most five bytes, as the binary format requires for a
/// `u32`.
fn read_u32(bytes: &[u8], at: &mut usize) -> Result<u32> {
    let mut value: u64 = 0;
    for shift in (0..35).step_by(7) {
        let Some(&byte) = bytes.get(*at) else {
            bail!("truncated LEB128");
        };
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return u32::try_from(value).context("LEB128 value exceeds u32");
        }
    }
    bail!("LEB128 longer than five bytes")
}

/// Copy `module` without its `name` section. Every other byte is copied
/// verbatim, in order.
pub fn strip(module: &[u8]) -> Result<Vec<u8>> {
    let sections = sections(module)?;
    let mut out = Vec::with_capacity(module.len());
    out.extend_from_slice(&module[..HEADER_LEN]);
    for section in sections.iter().filter(|s| !s.is_name()) {
        out.extend_from_slice(&module[section.range.clone()]);
    }
    Ok(out)
}

/// What [`verify`] found, for the build log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub original_len: usize,
    pub stripped_len: usize,
    /// Bytes of `name` section removed, headers included.
    pub name_len: usize,
    pub code_len: usize,
    pub data_len: usize,
    /// Every section kept, in order, with its size.
    pub kept: Vec<(String, usize)>,
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "module: {} -> {} bytes; removed {NAME_SECTION} section: {} bytes",
            self.original_len, self.stripped_len, self.name_len
        )?;
        writeln!(
            f,
            "code: {} bytes, data: {} bytes (both byte-identical)",
            self.code_len, self.data_len
        )?;
        let kept: Vec<String> = self
            .kept
            .iter()
            .map(|(label, len)| format!("{label} {len}"))
            .collect();
        write!(f, "kept, unchanged and in order: {}", kept.join(", "))
    }
}

/// Prove that `stripped` is `original` minus its `name` section and nothing
/// else.
///
/// Fails if the original has no `name` section (there would be nothing to
/// strip, so the step is either obsolete or looking at the wrong file), if the
/// stripped module still has one, or if any other section was added, removed,
/// reordered or changed by even one byte. The code and data sections are
/// named in the error because they are the ones that matter most.
pub fn verify(original: &[u8], stripped: &[u8]) -> Result<Report> {
    ensure!(
        original[..HEADER_LEN.min(original.len())] == stripped[..HEADER_LEN.min(stripped.len())],
        "module headers differ"
    );
    let before = sections(original).context("reading the original module")?;
    let after = sections(stripped).context("reading the stripped module")?;

    let names: Vec<&Section> = before.iter().filter(|s| s.is_name()).collect();
    ensure!(
        !names.is_empty(),
        "the original module has no `{NAME_SECTION}` section, so there is nothing to strip; \
         the toolchain no longer emits it, or this is not the debug frontend build"
    );
    ensure!(
        !after.iter().any(Section::is_name),
        "the stripped module still has a `{NAME_SECTION}` section"
    );

    let expected: Vec<&Section> = before.iter().filter(|s| !s.is_name()).collect();
    ensure!(
        expected.len() == after.len(),
        "section count changed beyond the `{NAME_SECTION}` section: expected {:?}, got {:?}",
        expected.iter().map(|s| s.label()).collect::<Vec<_>>(),
        after.iter().map(Section::label).collect::<Vec<_>>()
    );
    for (want, got) in expected.iter().zip(&after) {
        ensure!(
            want.id == got.id && want.custom_name == got.custom_name,
            "sections reordered or replaced: expected {}, found {}",
            want.label(),
            got.label()
        );
        ensure!(
            original[want.range.clone()] == stripped[got.range.clone()],
            "the {} section changed ({} -> {} bytes); only the `{NAME_SECTION}` section may be \
             removed, and nothing may be rewritten",
            want.label(),
            want.len(),
            got.len()
        );
    }

    let total = |id: u8| -> usize {
        after
            .iter()
            .filter(|s| s.id == id && s.custom_name.is_none())
            .map(Section::len)
            .sum()
    };
    let code_len = total(CODE);
    ensure!(code_len > 0, "the module has no code section");

    Ok(Report {
        original_len: original.len(),
        stripped_len: stripped.len(),
        name_len: names.iter().map(|s| s.len()).sum(),
        code_len,
        data_len: total(DATA),
        kept: after.iter().map(|s| (s.label(), s.len())).collect(),
    })
}

/// Whether a module still carries a `name` section.
pub fn has_name_section(module: &[u8]) -> Result<bool> {
    Ok(sections(module)?.iter().any(Section::is_name))
}

/// The Subresource Integrity value Trunk writes for a file: `sha384-<base64>`.
pub fn integrity(bytes: &[u8]) -> String {
    let digest = Sha384::digest(bytes);
    format!(
        "sha384-{}",
        base64::engine::general_purpose::STANDARD.encode(digest)
    )
}

/// Move Trunk's integrity pin in `index.html` from the original module to the
/// stripped one. Exactly one `integrity="<old>"` must exist, and nothing else
/// in the page changes.
pub fn replace_integrity(html: &str, original: &[u8], stripped: &[u8]) -> Result<String> {
    let old = format!("integrity=\"{}\"", integrity(original));
    let new = format!("integrity=\"{}\"", integrity(stripped));
    let found = html.matches(&old).count();
    ensure!(
        found == 1,
        "index.html pins the frontend module with {old} {found} times, expected once; \
         Trunk's output changed, so this step needs revisiting"
    );
    Ok(html.replacen(&old, &new, 1))
}

/// Whether `index.html` pins exactly this module.
pub fn pins(html: &str, module: &[u8]) -> bool {
    html.matches(&format!("integrity=\"{}\"", integrity(module)))
        .count()
        == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leb(mut value: usize, out: &mut Vec<u8>) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    fn section(id: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![id];
        leb(payload.len(), &mut out);
        out.extend_from_slice(payload);
        out
    }

    fn custom(name: &str, body: &[u8]) -> Vec<u8> {
        let mut payload = Vec::new();
        leb(name.len(), &mut payload);
        payload.extend_from_slice(name.as_bytes());
        payload.extend_from_slice(body);
        section(CUSTOM, &payload)
    }

    /// Shaped like wasm-bindgen output: standard sections, then `name`, then
    /// `producers` and `target_features`. The `name` body is large enough to
    /// need a multi-byte size.
    fn module() -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(MAGIC);
        m.extend_from_slice(VERSION);
        m.extend(section(1, &[1, 0x60, 0, 0]));
        m.extend(section(3, &[1, 0]));
        m.extend(section(CODE, &[1, 2, 0, 0x0b]));
        m.extend(section(DATA, &[1, 0, 0x41, 0, 0x0b, 3, b'a', b'b', b'c']));
        m.extend(custom(NAME_SECTION, &[0x5a; 300]));
        m.extend(custom("producers", b"rustc"));
        m.extend(custom("target_features", b"+mutable-globals"));
        m
    }

    fn replace_section(m: &[u8], label: &str, new: Vec<u8>) -> Vec<u8> {
        let mut out = m[..HEADER_LEN].to_vec();
        for s in sections(m).unwrap() {
            if s.label() == label {
                out.extend_from_slice(&new);
            } else {
                out.extend_from_slice(&m[s.range]);
            }
        }
        out
    }

    #[test]
    fn strip_removes_only_the_name_section() {
        let original = module();
        let stripped = strip(&original).unwrap();

        assert!(has_name_section(&original).unwrap());
        assert!(!has_name_section(&stripped).unwrap());

        let report = verify(&original, &stripped).unwrap();
        assert_eq!(report.original_len - report.stripped_len, report.name_len);
        assert_eq!(
            report
                .kept
                .iter()
                .map(|(l, _)| l.as_str())
                .collect::<Vec<_>>(),
            [
                "type",
                "function",
                "code",
                "data",
                "custom:producers",
                "custom:target_features"
            ]
        );
    }

    #[test]
    fn a_second_pass_has_nothing_to_strip() {
        let once = strip(&module()).unwrap();
        let twice = strip(&once).unwrap();
        assert_eq!(once, twice);
        let err = verify(&once, &twice).unwrap_err().to_string();
        assert!(err.contains("nothing to strip"), "{err}");
    }

    #[test]
    fn verify_refuses_a_changed_code_section() {
        let original = module();
        let optimised = replace_section(
            &strip(&original).unwrap(),
            "code",
            section(CODE, &[1, 2, 0, 0x01]),
        );
        let err = verify(&original, &optimised).unwrap_err().to_string();
        assert!(err.contains("code section changed"), "{err}");
    }

    #[test]
    fn verify_refuses_a_changed_data_section() {
        let original = module();
        let changed = replace_section(
            &strip(&original).unwrap(),
            "data",
            section(DATA, &[1, 0, 0x41, 0, 0x0b, 3, b'a', b'b', b'd']),
        );
        let err = verify(&original, &changed).unwrap_err().to_string();
        assert!(err.contains("data section changed"), "{err}");
    }

    #[test]
    fn verify_refuses_losing_another_custom_section() {
        let original = module();
        let over_stripped = replace_section(&strip(&original).unwrap(), "custom:producers", vec![]);
        let err = verify(&original, &over_stripped).unwrap_err().to_string();
        assert!(err.contains("section count changed"), "{err}");
    }

    #[test]
    fn verify_refuses_a_remaining_name_section() {
        let original = module();
        let err = verify(&original, &original).unwrap_err().to_string();
        assert!(err.contains("still has"), "{err}");
    }

    #[test]
    fn verify_refuses_reordered_sections() {
        let original = module();
        let stripped = strip(&original).unwrap();
        let parts = sections(&stripped).unwrap();
        let mut reordered = stripped[..HEADER_LEN].to_vec();
        for s in parts.iter().rev() {
            reordered.extend_from_slice(&stripped[s.range.clone()]);
        }
        let err = verify(&original, &reordered).unwrap_err().to_string();
        assert!(err.contains("reordered"), "{err}");
    }

    #[test]
    fn the_integrity_pin_moves_to_the_stripped_module_and_nothing_else_changes() {
        let original = module();
        let stripped = strip(&original).unwrap();
        let other = format!("integrity=\"{}\"", integrity(b"styles"));
        let html = format!(
            "<link rel=\"stylesheet\" {other}>\n<link rel=\"preload\" \
             integrity=\"{}\" as=\"fetch\">\n",
            integrity(&original)
        );

        let updated = replace_integrity(&html, &original, &stripped).unwrap();

        assert!(pins(&updated, &stripped));
        assert!(!pins(&updated, &original));
        assert!(updated.contains(&other));
        assert_eq!(
            updated.replace(&integrity(&stripped), &integrity(&original)),
            html
        );
    }

    #[test]
    fn the_integrity_pin_must_be_found_exactly_once() {
        let original = module();
        let stripped = strip(&original).unwrap();
        let pin = format!("integrity=\"{}\"", integrity(&original));

        let err = replace_integrity("<html></html>", &original, &stripped)
            .unwrap_err()
            .to_string();
        assert!(err.contains("0 times"), "{err}");
        let err = replace_integrity(&format!("{pin}{pin}"), &original, &stripped)
            .unwrap_err()
            .to_string();
        assert!(err.contains("2 times"), "{err}");
    }

    #[test]
    fn sections_rejects_malformed_input() {
        assert!(sections(b"not wasm").is_err());
        let mut truncated = module();
        truncated.truncate(truncated.len() - 3);
        assert!(sections(&truncated).is_err());
        let mut bad_version = module();
        bad_version[4] = 2;
        assert!(sections(&bad_version).is_err());
    }
}
