// SPDX-License-Identifier: MPL-2.0
//! Which published release the updater should offer, found through GitHub's
//! public REST API.
//!
//! GitHub's `releases/latest` answers one question only: the newest
//! non-draft, non-prerelease release by creation time. It cannot select a
//! prerelease, and it does not rank by version. So the updater lists the
//! releases, applies the channel's rule, takes the highest semantic version,
//! and then asks the plugin to check that release's own `latest.json`. The
//! plugin still verifies the signature of whatever it downloads.
//!
//! Selection is pure and lives in [`select`]. Fetching is bounded: a fixed
//! timeout, a fixed number of pages, and no credentials. The public API allows
//! sixty unauthenticated requests an hour per address, so the caller caches a
//! successful feed for [`CACHE_TTL`].

use crate::release_channel::{PlainVersion, ReleaseChannel};
use serde::Deserialize;
use std::sync::Once;
use std::time::Duration;

pub const REPOSITORY: &str = "BarraDev/slashit";
/// The releases listing the application reads.
pub const RELEASES_API: &str = "https://api.github.com/repos/BarraDev/slashit/releases";
/// The updater manifest every release carries, as `tauri-action` uploads it.
pub const MANIFEST_ASSET: &str = "latest.json";
const PAGE_SIZE: usize = 100;
/// Three pages: the newest three hundred releases. Enough for the foreseeable
/// history, and a bound on how many requests one check can make.
const MAX_PAGES: usize = 3;
const MAX_PAGE_BYTES: usize = 4 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a successful feed is reused before the API is asked again.
pub const CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// One release, reduced to what selection needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub tag: String,
    /// `None` when the tag is not exactly `v` followed by a plain version.
    pub version: Option<PlainVersion>,
    pub prerelease: bool,
    pub draft: bool,
    pub has_manifest: bool,
}

/// The releases one listing returned, newest first as GitHub orders them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Feed {
    pub candidates: Vec<Candidate>,
    /// Entries that were not valid release objects. Counted, not inspected.
    pub malformed: usize,
    /// True when the page limit ended the listing while releases remained.
    pub truncated: bool,
}

/// The release the updater will ask about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chosen {
    pub tag: String,
    pub version: PlainVersion,
    pub manifest_url: String,
}

/// The outcome of selection: at most one release to offer, plus anything the
/// person should be told that did not stop the choice.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    pub chosen: Option<Chosen>,
    pub notices: Vec<String>,
}

/// Why a feed could not be read. The caller decides how each class reads to the
/// person; none of them means "already up to date".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedError {
    Unreachable(String),
    RateLimited,
    Status(u16),
    Malformed(String),
}

/// The updater manifest for a release tag, at the URL the release publishes.
pub fn manifest_url(tag: &str) -> String {
    format!("https://github.com/{REPOSITORY}/releases/download/{tag}/{MANIFEST_ASSET}")
}

/// The tag every release must carry: `v` followed by its plain version.
pub fn tag_for(version: PlainVersion) -> String {
    format!("v{version}")
}

/// Choose the release to offer.
///
/// Drafts are never offered. The stable channel offers only releases GitHub
/// does not mark as prereleases; the prerelease channel offers both kinds. Among
/// the eligible releases newer than `current`, the highest version wins, whatever
/// position GitHub listed it in. A release without a manifest is skipped with a
/// notice, so it cannot hide an older release that does have one. Nothing at or
/// below `current` is offered, so there are no downgrades.
pub fn select(feed: &Feed, channel: ReleaseChannel, current: PlainVersion) -> Selection {
    let mut notices = Vec::new();
    if feed.malformed > 0 {
        notices.push(format!(
            "{} release entries from GitHub could not be read and were ignored.",
            feed.malformed
        ));
    }
    if feed.truncated {
        notices.push(format!(
            "Only the newest {} releases were examined.",
            PAGE_SIZE * MAX_PAGES
        ));
    }

    let mut best: Option<(PlainVersion, &Candidate)> = None;
    for candidate in &feed.candidates {
        if candidate.draft {
            continue;
        }
        if channel == ReleaseChannel::Stable && candidate.prerelease {
            continue;
        }
        let Some(version) = candidate.version else {
            notices.push(format!(
                "The release {} is not named v<major>.<minor>.<patch>, so it was ignored.",
                candidate.tag
            ));
            continue;
        };
        if version <= current {
            continue;
        }
        if !candidate.has_manifest {
            notices.push(format!(
                "The release {} is newer than this build but has no {MANIFEST_ASSET}, so it cannot be installed.",
                candidate.tag
            ));
            continue;
        }
        if best.is_none_or(|(held, _)| version > held) {
            best = Some((version, candidate));
        }
    }

    Selection {
        chosen: best.map(|(version, candidate)| Chosen {
            tag: candidate.tag.clone(),
            version,
            manifest_url: manifest_url(&candidate.tag),
        }),
        notices,
    }
}

/// Refuse a manifest that names a different version than the release it was
/// published under. Returns the message to show.
pub fn check_manifest_version(chosen: &Chosen, manifest_version: &str) -> Result<(), String> {
    if manifest_version == chosen.version.to_string() {
        return Ok(());
    }
    Err(format!(
        "The release {} publishes a manifest for version {manifest_version}, not {}.",
        chosen.tag, chosen.version
    ))
}

// --- Parsing ----------------------------------------------------------------

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
}

/// One page of the listing: its candidates, how many entries it held, and how
/// many of those were not valid release objects.
pub fn parse_page(json: &[u8]) -> Result<(Vec<Candidate>, usize, usize), FeedError> {
    let entries: Vec<serde_json::Value> =
        serde_json::from_slice(json).map_err(|error| FeedError::Malformed(error.to_string()))?;
    let count = entries.len();
    let mut candidates = Vec::with_capacity(count);
    let mut malformed = 0;
    for entry in entries {
        let Ok(release) = serde_json::from_value::<ApiRelease>(entry) else {
            malformed += 1;
            continue;
        };
        let version = release
            .tag_name
            .strip_prefix('v')
            .and_then(PlainVersion::parse)
            .filter(|version| tag_for(*version) == release.tag_name);
        candidates.push(Candidate {
            has_manifest: release.assets.iter().any(|asset| asset.name == MANIFEST_ASSET),
            tag: release.tag_name,
            version,
            prerelease: release.prerelease,
            draft: release.draft,
        });
    }
    Ok((candidates, count, malformed))
}

// --- Fetching ---------------------------------------------------------------

/// Install the TLS provider once. The plugin installs the same provider before
/// its own requests, so the two agree; this covers a request made first.
fn install_tls_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// An HTTP client for the releases API: HTTPS only, bounded time, no
/// credentials, and a user agent that names the version asking.
pub fn client() -> Result<reqwest::Client, FeedError> {
    install_tls_provider();
    reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent(format!("slashit-updater/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| FeedError::Unreachable(error.to_string()))
}

/// List the releases at `listing`, newest first, over at most [`MAX_PAGES`]
/// pages. Stops early when a page is short. `listing` is [`RELEASES_API`] in the
/// application; tests point it at a local server.
pub async fn fetch_feed(client: &reqwest::Client, listing: &str) -> Result<Feed, FeedError> {
    let mut feed = Feed::default();
    for page in 1..=MAX_PAGES {
        let response = client
            .get(format!("{listing}?per_page={PAGE_SIZE}&page={page}"))
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .map_err(describe_transport)?;
        let status = response.status();
        let exhausted = response
            .headers()
            .get("x-ratelimit-remaining")
            .is_some_and(|value| value == "0");
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || (status == reqwest::StatusCode::FORBIDDEN && exhausted)
        {
            return Err(FeedError::RateLimited);
        }
        if !status.is_success() {
            return Err(FeedError::Status(status.as_u16()));
        }
        let body = response.bytes().await.map_err(describe_transport)?;
        if body.len() > MAX_PAGE_BYTES {
            return Err(FeedError::Malformed(format!(
                "the releases listing is larger than {MAX_PAGE_BYTES} bytes"
            )));
        }
        let (candidates, count, malformed) = parse_page(&body)?;
        feed.candidates.extend(candidates);
        feed.malformed += malformed;
        if count < PAGE_SIZE {
            return Ok(feed);
        }
        if page == MAX_PAGES {
            feed.truncated = true;
        }
    }
    Ok(feed)
}

fn describe_transport(error: reqwest::Error) -> FeedError {
    if error.is_timeout() {
        FeedError::Unreachable("the request timed out".to_string())
    } else {
        FeedError::Unreachable(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(text: &str) -> PlainVersion {
        PlainVersion::parse(text).unwrap()
    }

    fn release(tag: &str, prerelease: bool) -> Candidate {
        Candidate {
            tag: tag.to_string(),
            version: tag.strip_prefix('v').and_then(PlainVersion::parse),
            prerelease,
            draft: false,
            has_manifest: true,
        }
    }

    fn feed(candidates: Vec<Candidate>) -> Feed {
        Feed {
            candidates,
            ..Feed::default()
        }
    }

    fn offered(selection: &Selection) -> Option<String> {
        selection.chosen.as_ref().map(|chosen| chosen.tag.clone())
    }

    /// The transition table from the release plan, one row per case.
    #[test]
    fn the_channel_transition_table_holds() {
        let stable = ReleaseChannel::Stable;
        let prerelease = ReleaseChannel::Prerelease;
        let cases = [
            // installed, channel, published (tag, prerelease), expected offer
            ("0.0.1", stable, vec![("v0.0.2", true)], None),
            ("0.0.1", prerelease, vec![("v0.0.2", true)], Some("v0.0.2")),
            ("0.0.2", prerelease, vec![("v0.0.3", false)], Some("v0.0.3")),
            ("0.0.3", stable, vec![("v0.0.4", true)], None),
            ("0.0.3", prerelease, vec![("v0.0.4", true)], Some("v0.0.4")),
            ("0.0.4", stable, vec![("v0.0.3", false)], None),
            ("0.0.4", stable, vec![("v0.0.5", false)], Some("v0.0.5")),
        ];
        for (installed, channel, published, expected) in cases {
            let candidates = published.into_iter().map(|(tag, pre)| release(tag, pre)).collect();
            let selection = select(&feed(candidates), channel, version(installed));
            assert_eq!(
                offered(&selection).as_deref(),
                expected,
                "installed {installed}, channel {}",
                channel.as_str()
            );
        }
    }

    #[test]
    fn the_highest_version_is_offered_not_the_first_listed() {
        let candidates = vec![release("v0.0.2", false), release("v0.0.10", false), release("v0.0.9", true)];
        let selection = select(&feed(candidates), ReleaseChannel::Prerelease, version("0.0.1"));
        assert_eq!(offered(&selection).as_deref(), Some("v0.0.10"));
    }

    #[test]
    fn a_stable_release_newer_than_a_prerelease_is_offered_to_prerelease_users() {
        let candidates = vec![release("v0.0.3", false), release("v0.0.2", true)];
        let selection = select(&feed(candidates), ReleaseChannel::Prerelease, version("0.0.2"));
        assert_eq!(offered(&selection).as_deref(), Some("v0.0.3"));
    }

    #[test]
    fn no_published_releases_offers_nothing_and_says_nothing_is_wrong() {
        let selection = select(&Feed::default(), ReleaseChannel::Stable, version("0.0.1"));
        assert_eq!(selection, Selection::default());
    }

    #[test]
    fn drafts_are_never_offered() {
        let mut draft = release("v0.0.2", false);
        draft.draft = true;
        let selection = select(&feed(vec![draft]), ReleaseChannel::Prerelease, version("0.0.1"));
        assert_eq!(offered(&selection), None);
        assert!(selection.notices.is_empty());
    }

    #[test]
    fn a_newer_release_without_a_manifest_is_skipped_with_a_notice() {
        let mut incomplete = release("v0.0.3", false);
        incomplete.has_manifest = false;
        let candidates = vec![incomplete, release("v0.0.2", false)];
        let selection = select(&feed(candidates), ReleaseChannel::Stable, version("0.0.1"));
        assert_eq!(offered(&selection).as_deref(), Some("v0.0.2"));
        assert_eq!(selection.notices.len(), 1);
        assert!(selection.notices[0].contains("v0.0.3"));
    }

    #[test]
    fn a_malformed_tag_is_ignored_with_a_notice_and_does_not_hide_a_valid_release() {
        let mut odd = release("release-next", false);
        odd.version = None;
        let candidates = vec![odd, release("v0.0.2", false)];
        let selection = select(&feed(candidates), ReleaseChannel::Stable, version("0.0.1"));
        assert_eq!(offered(&selection).as_deref(), Some("v0.0.2"));
        assert!(selection.notices.iter().any(|notice| notice.contains("release-next")));
    }

    #[test]
    fn unreadable_entries_and_a_truncated_listing_are_reported() {
        let truncated = Feed {
            candidates: vec![release("v0.0.2", false)],
            malformed: 2,
            truncated: true,
        };
        let selection = select(&truncated, ReleaseChannel::Stable, version("0.0.1"));
        assert_eq!(offered(&selection).as_deref(), Some("v0.0.2"));
        assert_eq!(selection.notices.len(), 2);
    }

    #[test]
    fn a_release_at_or_below_the_installed_version_is_never_offered() {
        let candidates = vec![release("v0.0.2", false), release("v0.0.1", false)];
        let selection = select(&feed(candidates), ReleaseChannel::Stable, version("0.0.2"));
        assert_eq!(selection, Selection::default());
    }

    #[test]
    fn the_manifest_url_is_the_release_asset_for_its_tag() {
        assert_eq!(
            manifest_url("v0.0.1"),
            "https://github.com/BarraDev/slashit/releases/download/v0.0.1/latest.json"
        );
    }

    #[test]
    fn a_manifest_naming_another_version_is_refused() {
        let chosen = Chosen {
            tag: "v0.0.2".to_string(),
            version: version("0.0.2"),
            manifest_url: manifest_url("v0.0.2"),
        };
        assert!(check_manifest_version(&chosen, "0.0.2").is_ok());
        let refused = check_manifest_version(&chosen, "0.0.9").unwrap_err();
        assert!(refused.contains("v0.0.2") && refused.contains("0.0.9"), "{refused}");
    }

    #[test]
    fn a_listing_page_yields_candidates_and_counts_malformed_entries() {
        let page = br#"[
            {"tag_name":"v0.0.2","draft":false,"prerelease":true,"assets":[{"name":"latest.json"}]},
            {"tag_name":"v0.0.3-1","draft":false,"prerelease":false,"assets":[]},
            {"draft":false},
            {"tag_name":"v0.0.4","draft":false,"prerelease":false,"assets":[{"name":"SlashIt.msi"}]}
        ]"#;
        let (candidates, count, malformed) = parse_page(page).unwrap();
        assert_eq!((count, malformed), (4, 1));
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].version, Some(version("0.0.2")));
        assert!(candidates[0].has_manifest && candidates[0].prerelease);
        assert_eq!(candidates[1].version, None, "a numeric suffix is not a plain version");
        assert!(!candidates[2].has_manifest, "an asset list without latest.json has no manifest");
    }

    #[test]
    fn a_listing_that_is_not_json_is_malformed() {
        assert!(matches!(parse_page(b"<html>"), Err(FeedError::Malformed(_))));
    }

    // --- Against a local HTTP server --------------------------------------
    //
    // The application's client is HTTPS-only, so these tests build a plain one
    // and point the listing at a socket the test owns.

    fn plain_client() -> reqwest::Client {
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::builder().timeout(Duration::from_secs(5)).build().unwrap()
    }

    /// One HTTP/1.1 response, closed after the body.
    fn response(status: &str, extra_headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// A local server that answers one connection per response, in order.
    fn serve(responses: Vec<String>) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let listing = format!("http://{}/repos/BarraDev/slashit/releases", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            for reply in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 8192];
                let _ = stream.read(&mut request);
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        (listing, handle)
    }

    /// A full page of `count` published releases, numbered from `first` patch.
    fn full_page(first: u64, count: usize) -> String {
        let entries: Vec<String> = (first..first + count as u64)
            .map(|patch| {
                format!(
                    r#"{{"tag_name":"v0.0.{patch}","draft":false,"prerelease":false,"assets":[{{"name":"latest.json"}}]}}"#
                )
            })
            .collect();
        format!("[{}]", entries.join(","))
    }

    #[tokio::test]
    async fn a_listing_is_read_page_by_page_until_a_short_page() {
        let (listing, server) = serve(vec![
            response("200 OK", "", &full_page(0, PAGE_SIZE)),
            response("200 OK", "", &full_page(PAGE_SIZE as u64, 3)),
        ]);
        let feed = fetch_feed(&plain_client(), &listing).await.unwrap();
        server.join().unwrap();
        assert_eq!(feed.candidates.len(), PAGE_SIZE + 3);
        assert!(!feed.truncated);
    }

    #[tokio::test]
    async fn a_listing_that_fills_every_page_is_marked_truncated() {
        let (listing, server) = serve(
            (0..MAX_PAGES as u64)
                .map(|page| response("200 OK", "", &full_page(page * 1000, PAGE_SIZE)))
                .collect(),
        );
        let feed = fetch_feed(&plain_client(), &listing).await.unwrap();
        server.join().unwrap();
        assert_eq!(feed.candidates.len(), PAGE_SIZE * MAX_PAGES);
        assert!(feed.truncated, "more releases may exist beyond the page limit");
    }

    #[tokio::test]
    async fn a_missing_listing_is_an_error_not_an_empty_feed() {
        let (listing, server) = serve(vec![response("404 Not Found", "", "{}")]);
        let error = fetch_feed(&plain_client(), &listing).await.unwrap_err();
        server.join().unwrap();
        assert_eq!(error, FeedError::Status(404));
    }

    #[tokio::test]
    async fn an_exhausted_quota_is_rate_limited_not_an_empty_feed() {
        let (listing, server) = serve(vec![response(
            "403 Forbidden",
            "x-ratelimit-remaining: 0\r\n",
            "{}",
        )]);
        let error = fetch_feed(&plain_client(), &listing).await.unwrap_err();
        server.join().unwrap();
        assert_eq!(error, FeedError::RateLimited);
    }

    #[tokio::test]
    async fn a_server_error_is_reported_as_a_status_not_as_no_releases() {
        let (listing, server) = serve(vec![response("503 Service Unavailable", "", "{}")]);
        let error = fetch_feed(&plain_client(), &listing).await.unwrap_err();
        server.join().unwrap();
        assert_eq!(error, FeedError::Status(503));
    }

    #[tokio::test]
    async fn a_listing_body_that_is_not_json_is_malformed() {
        let (listing, server) = serve(vec![response("200 OK", "", "<html>maintenance</html>")]);
        let error = fetch_feed(&plain_client(), &listing).await.unwrap_err();
        server.join().unwrap();
        assert!(matches!(error, FeedError::Malformed(_)), "{error:?}");
    }

    #[tokio::test]
    async fn a_listing_that_cannot_be_reached_is_unreachable() {
        // Bind, learn the port, and release it: nothing listens there now.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let listing = format!("http://127.0.0.1:{port}/releases");
        let error = fetch_feed(&plain_client(), &listing).await.unwrap_err();
        assert!(matches!(error, FeedError::Unreachable(_)), "{error:?}");
    }
}
