// SPDX-License-Identifier: MPL-2.0
//! Task references typed on the command line.
//!
//! A reference is a full Task UUID or a unique hexadecimal prefix of at least
//! eight characters, the same short ID the app shows. A prefix is resolved
//! against the Task records the instance returns for `ListTasks`; it is never
//! guessed, and an ambiguous prefix names every match instead of picking one.

use serde_json::Value;

/// The fewest characters a prefix may have.
pub const MIN_PREFIX_LEN: usize = 8;

/// What a typed reference is, before any Task is looked up.
#[derive(Debug, PartialEq, Eq)]
pub enum TaskRef {
    /// A complete UUID, lowercased. It is sent as is.
    Full(String),
    /// A valid prefix, lowercased. It needs the Task list to become an ID.
    Prefix(String),
}

fn is_full_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Classifies `input`, or says why it is not a Task reference.
pub fn parse(input: &str) -> Result<TaskRef, String> {
    let lower = input.trim().to_ascii_lowercase();
    if is_full_uuid(&lower) {
        return Ok(TaskRef::Full(lower));
    }
    // Dashes are allowed so a prefix may be copied from a longer UUID.
    if lower.is_empty() || !lower.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return Err(format!(
            "'{input}' is not a Task ID: use a full UUID or a hexadecimal prefix of at least {MIN_PREFIX_LEN} characters"
        ));
    }
    if lower.len() < MIN_PREFIX_LEN {
        return Err(format!(
            "'{input}' is too short: a Task ID prefix needs at least {MIN_PREFIX_LEN} characters"
        ));
    }
    if lower.len() >= 36 {
        return Err(format!("'{input}' is not a Task ID: it is longer than a UUID or malformed"));
    }
    Ok(TaskRef::Prefix(lower))
}

/// Turns a valid prefix into the one full Task ID it names.
pub fn resolve_prefix(prefix: &str, tasks: &Value) -> Result<String, String> {
    let empty = Vec::new();
    let tasks = tasks.as_array().unwrap_or(&empty);
    let matches: Vec<&Value> = tasks
        .iter()
        .filter(|t| {
            t["id"]
                .as_str()
                .is_some_and(|id| id.to_ascii_lowercase().starts_with(prefix))
        })
        .collect();
    match matches.as_slice() {
        [] => Err(format!("no Task has an ID starting with '{prefix}'")),
        [one] => Ok(one["id"].as_str().unwrap_or_default().to_string()),
        many => {
            let mut message =
                format!("'{prefix}' matches {} Tasks; use a longer prefix:", many.len());
            for t in many {
                message.push_str(&format!(
                    "\n  {}  {}  {}",
                    t["id"].as_str().unwrap_or("?"),
                    t["status"].as_str().unwrap_or("?"),
                    t["title"].as_str().unwrap_or("?"),
                ));
            }
            Err(message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FULL: &str = "6f96fefa-0000-4000-8000-000000000003";

    fn tasks() -> Value {
        json!([
            {"id": FULL, "title": "one", "status": "backlog"},
            {"id": "aaaaaaaa-0000-4000-8000-000000000001", "title": "two", "status": "queue"},
            {"id": "aaaaaaaa-1111-4000-8000-000000000002", "title": "three", "status": "done"},
        ])
    }

    #[test]
    fn full_uuid_is_accepted_and_lowercased() {
        assert_eq!(parse(FULL), Ok(TaskRef::Full(FULL.into())));
        assert_eq!(parse(&FULL.to_uppercase()), Ok(TaskRef::Full(FULL.into())));
    }

    #[test]
    fn malformed_references_are_refused() {
        assert!(parse("6f96fef").unwrap_err().contains("too short"));
        assert!(parse("6f96fefz").unwrap_err().contains("not a Task ID"));
        assert!(parse("").is_err());
        assert!(parse("6f96fefa-0000-4000-8000-0000000000031").is_err());
    }

    #[test]
    fn unique_prefix_resolves_in_either_case() {
        for p in ["6f96fefa", "6F96FEFA"] {
            let TaskRef::Prefix(p) = parse(p).unwrap() else { panic!() };
            assert_eq!(resolve_prefix(&p, &tasks()).unwrap(), FULL);
        }
    }

    #[test]
    fn unknown_prefix_is_refused() {
        assert!(resolve_prefix("bbbbbbbb", &tasks()).unwrap_err().contains("no Task"));
    }

    #[test]
    fn ambiguous_prefix_lists_every_match() {
        let err = resolve_prefix("aaaaaaaa", &tasks()).unwrap_err();
        assert!(err.contains("matches 2 Tasks"));
        assert!(err.contains("two") && err.contains("three"));
        assert!(resolve_prefix("aaaaaaaa-1", &tasks()).is_ok());
    }
}
