//! Free text as a timeline entry may keep it.
//!
//! Every piece of text an entry stores -- a tool call's detail, a failure's
//! reason -- comes from something SlashIt does not control: a command the
//! agent wrote, a search it ran, a line a subprocess printed. [`record`],
//! [`record_tool`] and [`append`] pass all of it through [`sanitize`], so no
//! caller can put a credential in the task file by forgetting to.
//!
//! Masking is best effort by construction: it recognizes the shapes
//! credentials usually take, not every secret. That is why what reaches it is
//! already a short excerpt -- a command's first line, a reason -- and never a
//! tool's full input or a transcript. When it is unsure, it masks.
//!
//! [`record`]: crate::record
//! [`record_tool`]: crate::record_tool
//! [`append`]: crate::append

use crate::one_line;

/// What a masked value is replaced with.
pub const MASK: &str = "***";

/// Words that mark an assignment, flag or header as carrying a secret when
/// its name contains one.
const SECRET_WORDS: &[&str] = &[
    "token", "secret", "password", "passwd", "pwd", "apikey", "api_key", "api-key", "auth", "credential",
    "cookie", "private_key", "access_key", "signature", "passphrase",
];

/// Parts of a name (split at `_`, `-` and `.`) that mark it as carrying a
/// secret: `DB_PASS`, `SMTP_PASS`, `OPENAI_KEY`, `Ocp-Apim-Subscription-Key`.
/// A part that ends in `key` counts too (`masterkey`).
const SECRET_NAME_PARTS: &[&str] = &["pass", "passcode", "key"];

/// Prefixes of well-known credential formats: GitHub, GitLab, OpenAI and
/// Anthropic, Slack, AWS, Google, npm, PyPI, Hugging Face, Stripe, and JWTs.
/// The second field is whether a match must also contain a digit, for
/// prefixes that ordinary names also start with (`npm_config_cache`,
/// `ASIA-Pacific`).
const SECRET_PREFIXES: &[(&str, bool)] = &[
    ("ghp_", false),
    ("gho_", false),
    ("ghu_", false),
    ("ghs_", false),
    ("ghr_", false),
    ("github_pat_", false),
    ("glpat-", false),
    ("sk-", true),
    ("sk_live_", false),
    ("rk_live_", false),
    ("xoxb-", false),
    ("xoxp-", false),
    ("xoxa-", false),
    ("xoxs-", false),
    ("AKIA", false),
    ("ASIA", true),
    ("AIza", false),
    ("ya29.", false),
    ("npm_", true),
    ("pypi-", false),
    ("hf_", true),
    ("eyJ", false),
];

/// How many characters past its prefix make a word a token. Fewer are still
/// a token when a truncation (`…`) cut it short.
const MIN_TOKEN_TAIL: usize = 9;

/// Authorization schemes: in a header, the credential follows them.
const SCHEMES: &[&str] = &["bearer", "basic", "token", "digest", "negotiate"];

/// Characters that can follow a URL inside one word without being part of
/// it.
const URL_CLOSERS: &[char] = &['"', '\'', ')', ']', '>', ',', ';'];

/// `text` as an entry keeps it: credentials masked (see [`redact`]), on one
/// line of at most `max` characters.
pub fn sanitize(text: &str, max: usize) -> String {
    one_line(&redact(text), max)
}

/// What masking does next, carried from one word to the following ones.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pending {
    Nothing,
    /// The next word is a secret; when it is only an authorization scheme,
    /// the one after it is too.
    NextWord,
    /// Every word up to and including the one that closes this quote is a
    /// secret: a quoted header value such as `"Authorization: Bearer x"`.
    UntilQuote(char),
    /// The next word is a secret if it is `user:password` (after `-u`).
    UserPassword,
    /// The next word is a secret if it looks like a credential rather than
    /// prose (after a standalone `Basic` or `token`).
    Credential,
}

/// `text` with whatever looks like a credential masked, words separated by
/// single spaces:
///
/// - the value of an assignment, flag or header whose name mentions a
///   secret (`API_KEY=x`, `DB_PASS=x`, `--password x`, `-H "X-Api-Key: x"`),
///   including every word of a quoted value and the credential after an
///   authorization scheme (`Authorization:Bearer x`, `Authorization: token x`);
/// - the same, inside the value of any other assignment or flag
///   (`--header=Authorization:Bearer x`, `--raw-field=password=x`);
/// - the word after a standalone `Bearer`, and after `Basic` or `token` when
///   it looks like a credential rather than prose;
/// - `user:password` after `-u`/`--user`, or glued to `-u`;
/// - a word containing a token in a well-known format (`ghp_…`, `sk-…`, a
///   JWT), or what a truncation left of one;
/// - every URL's user information, and everything from a URL's query or
///   fragment on, any of which can carry a credential or a signature.
pub fn redact(text: &str) -> String {
    let mut pending = Pending::Nothing;
    text.split_whitespace().map(|word| redact_word(word, &mut pending)).collect::<Vec<_>>().join(" ")
}

/// One word of [`redact`], given what the words before it left pending.
fn redact_word(word: &str, pending: &mut Pending) -> String {
    let bare = word.trim_matches(|c| c == '"' || c == '\'');
    let lower = bare.to_ascii_lowercase();
    match std::mem::replace(pending, Pending::Nothing) {
        Pending::UntilQuote(quote) => {
            if word.contains(quote) {
                return format!("{MASK}{quote}");
            }
            *pending = Pending::UntilQuote(quote);
            return MASK.to_string();
        }
        Pending::NextWord => {
            *pending = match open_quote(word) {
                Some(quote) => Pending::UntilQuote(quote),
                None if SCHEMES.contains(&lower.as_str()) => Pending::NextWord,
                None => Pending::Nothing,
            };
            return masked(word);
        }
        Pending::UserPassword if bare.contains(':') => return masked(word),
        Pending::Credential if looks_like_credential(bare) => return masked(word),
        Pending::UserPassword | Pending::Credential | Pending::Nothing => {}
    }

    if lower == "bearer" {
        *pending = value_follows(word);
        return word.to_string();
    }
    if lower == "basic" || lower == "token" {
        *pending = match open_quote(word) {
            Some(quote) => Pending::UntilQuote(quote),
            None => Pending::Credential,
        };
        return word.to_string();
    }
    if matches!(bare, "-u" | "--user") {
        *pending = Pending::UserPassword;
        return word.to_string();
    }
    // `-ualice:hunter2`
    if bare.starts_with("-u") && !bare.starts_with("--") && bare.contains(':') {
        return format!("{}-u{MASK}{}", leading_quote(word), closing_quote(word));
    }
    // `https://x/?token=y` is a URL, whose query is dropped below, not an
    // assignment.
    if let Some((name, value)) = word.split_once('=').filter(|(name, _)| !name.contains("://")) {
        let name_bare = name.trim_start_matches(['"', '\'']);
        if names_secret(name_bare) || name_bare == "--user" && value.contains(':') {
            *pending = match open_quote(word) {
                Some(quote) => Pending::UntilQuote(quote),
                None if value.is_empty() || is_scheme(value.trim_matches(['"', '\''])) => Pending::NextWord,
                None => Pending::Nothing,
            };
            return format!("{name}={}{MASK}{}", leading_quote(value), closing_quote(word));
        }
        // Any other assignment or `--flag=value`: the value is masked as a
        // word of its own would be.
        return format!("{name}={}", redact_word(value, pending));
    }
    if bare.starts_with('-') && names_secret(bare) {
        *pending = value_follows(word);
        return word.to_string();
    }
    if let Some((name, value)) = bare.split_once(':') {
        if !bare.contains("://") && names_secret(name) {
            if value.is_empty() {
                // `Authorization: Bearer x`: the value is in the following
                // words.
                *pending = value_follows(word);
                return word.to_string();
            }
            // `Authorization:Bearer x`, `X-Api-Key:x`.
            *pending = match open_quote(word) {
                Some(quote) => Pending::UntilQuote(quote),
                None if is_scheme(value) => Pending::NextWord,
                None => Pending::Nothing,
            };
            return format!("{}{name}:{MASK}{}", leading_quote(word), closing_quote(word));
        }
    }
    // A URL loses its query first, so a token there does not cost the rest
    // of the URL.
    let word = without_url_secrets(word);
    if has_known_token(&word) {
        masked(&word)
    } else {
        word
    }
}

fn names_secret(word: &str) -> bool {
    // `auth` names a secret in `Authorization` or `X-Auth`, but `--author`
    // is only a name.
    let word = word.to_ascii_lowercase().replace("authoriz", "auth").replace("authoris", "auth").replace("author", "");
    SECRET_WORDS.iter().any(|s| word.contains(s))
        || word
            .split(['_', '-', '.'])
            .any(|part| SECRET_NAME_PARTS.contains(&part) || part.len() > 3 && part.ends_with("key"))
}

fn is_scheme(word: &str) -> bool {
    SCHEMES.contains(&word.to_ascii_lowercase().as_str())
}

/// Whether some run of token characters in `word` starts with a well-known
/// credential prefix and is long enough to be one, or was cut short by a
/// truncation.
fn has_known_token(word: &str) -> bool {
    word.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))).any(|run| {
        let end = run.as_ptr() as usize - word.as_ptr() as usize + run.len();
        let truncated = word[end..].starts_with('…');
        SECRET_PREFIXES.iter().any(|(prefix, needs_digit)| {
            let tail = run.len().saturating_sub(prefix.len());
            run.starts_with(prefix)
                && (tail >= MIN_TOKEN_TAIL || truncated && tail > 0)
                && (!needs_digit || run[prefix.len()..].chars().any(|c| c.is_ascii_digit()))
        })
    })
}

/// Whether a word after `Basic` or `token` is a credential rather than the
/// next word of a sentence ("token parser"): long, with no character a
/// credential would not use, and not only letters.
fn looks_like_credential(word: &str) -> bool {
    word.len() >= 8
        && word.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '+' | '/' | '=' | '-'))
        && !word.chars().all(|c| c.is_ascii_alphabetic())
}

/// What masking does after a word that says a value follows: every word of
/// a quote it opens, otherwise the next word.
fn value_follows(word: &str) -> Pending {
    match open_quote(word) {
        Some(quote) => Pending::UntilQuote(quote),
        None => Pending::NextWord,
    }
}

/// The quote `word` opens and does not close, if any.
fn open_quote(word: &str) -> Option<char> {
    ['"', '\''].into_iter().find(|q| word.matches(*q).count() % 2 == 1)
}

fn leading_quote(word: &str) -> &str {
    &word[..word.len() - word.trim_start_matches(['"', '\'']).len()]
}

fn closing_quote(word: &str) -> &str {
    let trimmed = word.trim_end_matches(['"', '\'']);
    if trimmed.len() == word.len() || open_quote(word).is_some() {
        ""
    } else {
        &word[trimmed.len()..]
    }
}

/// `word` masked whole, keeping the quotes around it.
fn masked(word: &str) -> String {
    let lead = leading_quote(word);
    let rest = &word[lead.len()..];
    let tail = &rest[rest.trim_end_matches(['"', '\'']).len()..];
    format!("{lead}{MASK}{tail}")
}

/// A word with every URL in it reduced to scheme, host and path: user
/// information is masked, and everything from the first query or fragment on
/// is dropped, except the quotes or brackets that close the word.
fn without_url_secrets(word: &str) -> String {
    let Some(first) = word.find("://") else {
        return word.to_string();
    };
    let (body, closers) = match word[first..].find(['?', '#']) {
        Some(at) => {
            let rest = &word[first + at..];
            (&word[..first + at], &rest[rest.trim_end_matches(URL_CLOSERS).len()..])
        }
        None => (word, ""),
    };
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(scheme) = rest.find("://") {
        let start = scheme + 3;
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let authority_end = after.find(|c: char| c == '/' || URL_CLOSERS.contains(&c)).unwrap_or(after.len());
        let authority = &after[..authority_end];
        match authority.rfind('@') {
            Some(at) => {
                out.push_str(MASK);
                out.push_str(&authority[at..]);
            }
            None => out.push_str(authority),
        }
        rest = &after[authority_end..];
    }
    out.push_str(rest);
    out.push_str(closers);
    out
}
