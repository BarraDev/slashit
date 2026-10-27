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
}

/// Where this build reads releases and manifests from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    /// The releases listing, with no query string.
    pub listing: String,
    /// The directory that holds each release's assets: `{download_base}/{tag}/latest.json`.
    pub download_base: String,
}

impl Endpoints {
    /// GitHub: the only endpoints a production build uses.
    pub fn published() -> Self {
        Self {
            listing: RELEASES_API.to_string(),
            download_base: format!("https://github.com/{REPOSITORY}/releases/download"),
        }
    }
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

/// The updater manifest for a release tag under `download_base`, which is
/// [`Endpoints::download_base`].
pub fn manifest_url(download_base: &str, tag: &str) -> String {
    format!("{download_base}/{tag}/{MANIFEST_ASSET}")
}

/// The endpoints this build reads. A production build always returns
/// [`Endpoints::published`]. A build with `updater-test-endpoints` takes its two
/// locations from the environment, and only loopback addresses are accepted.
pub fn endpoints() -> Result<Endpoints, String> {
    #[cfg(feature = "updater-test-endpoints")]
    {
        test_endpoints::from_environment()
    }
    #[cfg(not(feature = "updater-test-endpoints"))]
    {
        Ok(Endpoints::published())
    }
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

/// An HTTP client for the releases listing `endpoints` names: bounded time, no
/// credentials, and a user agent that names the version asking. It is HTTPS
/// only, except in a test build with a configured loopback override.
pub fn client(endpoints: &Endpoints) -> Result<reqwest::Client, FeedError> {
    install_tls_provider();
    transport(reqwest::Client::builder(), endpoints)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .user_agent(format!("slashit-updater/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| FeedError::Unreachable(error.to_string()))
}

/// The redirect limit for every client this module configures. reqwest enforces
/// `limited(n)` as `previous.len() > n`.
const MAX_REDIRECTS: usize = 5;

/// The production transport: HTTPS only, at most [`MAX_REDIRECTS`] redirects.
/// `https_only` also refuses a redirect whose target is plain HTTP. It limits
/// the scheme and not the host, so a redirect to another HTTPS host, such as
/// GitHub's release-asset CDN, is still followed.
fn https_only_transport(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    builder
        .https_only(true)
        .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
}

#[cfg(not(feature = "updater-test-endpoints"))]
fn transport(builder: reqwest::ClientBuilder, _endpoints: &Endpoints) -> reqwest::ClientBuilder {
    https_only_transport(builder)
}

/// A build with the test seam uses the production transport unless both test
/// locations are configured. With an override, the listing client may reach a
/// loopback address over plain HTTP, bypasses system proxies, and follows only
/// redirects that stay on loopback (see [`check_payload_location`] for the
/// payload).
#[cfg(feature = "updater-test-endpoints")]
fn transport(builder: reqwest::ClientBuilder, endpoints: &Endpoints) -> reqwest::ClientBuilder {
    if *endpoints == Endpoints::published() {
        return https_only_transport(builder);
    }
    builder.no_proxy().redirect(reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() > MAX_REDIRECTS {
            attempt.error("too many redirects")
        } else if test_endpoints::redirect_allowed(attempt.previous(), attempt.url()) {
            attempt.follow()
        } else {
            attempt.error("the test build refuses a redirect it was not configured to follow")
        }
    }))
}

/// The client policy for the updater plugin's own requests: the manifest, and
/// the payload download, which the plugin makes with the same client. With no
/// test override it is the production transport, the same as the listing client.
pub fn plugin_client_policy(
    endpoints: &Endpoints,
) -> impl Fn(reqwest::ClientBuilder) -> reqwest::ClientBuilder + Send + Sync + 'static {
    let endpoints = endpoints.clone();
    move |builder| transport(builder, &endpoints)
}

/// The plugin takes the payload URL from the manifest and requests it without
/// checking its location, and the redirect policy does not see the first
/// request. So the location is checked here, before any download. Production
/// payloads must be HTTPS, on any host: real manifests name GitHub's API asset
/// URLs, which redirect to a CDN host.
pub fn check_payload_location(endpoints: &Endpoints, payload: &str) -> Result<(), String> {
    let allowed = reqwest::Url::parse(payload).is_ok_and(|url| payload_allowed(endpoints, &url));
    if allowed {
        Ok(())
    } else {
        Err(format!("the update refuses a payload at {payload}, which is not an allowed download location"))
    }
}

#[cfg(not(feature = "updater-test-endpoints"))]
fn payload_allowed(_endpoints: &Endpoints, url: &reqwest::Url) -> bool {
    url.scheme() == "https"
}

/// With a configured override, the payload must come from the configured
/// download server: the same scheme, host and port as `download_base`, which is
/// loopback. A build with no override keeps the production rule.
#[cfg(feature = "updater-test-endpoints")]
fn payload_allowed(endpoints: &Endpoints, url: &reqwest::Url) -> bool {
    if *endpoints == Endpoints::published() {
        return url.scheme() == "https";
    }
    reqwest::Url::parse(&endpoints.download_base)
        .is_ok_and(|base| url.origin() == base.origin() && test_endpoints::is_loopback(url))
}

/// Test-only endpoint overrides. Compiled only with `updater-test-endpoints`.
#[cfg(feature = "updater-test-endpoints")]
mod test_endpoints {
    use super::Endpoints;
    use std::net::IpAddr;

    /// The releases listing, for example `http://127.0.0.1:8080/repos/x/y/releases`.
    pub const LISTING_VAR: &str = "SLASHIT_UPDATER_TEST_RELEASES_API";
    /// The directory of release downloads, for example `http://127.0.0.1:8080/download`.
    pub const DOWNLOAD_VAR: &str = "SLASHIT_UPDATER_TEST_DOWNLOAD_BASE";

    pub fn from_environment() -> Result<Endpoints, String> {
        resolve(read_var(LISTING_VAR)?.as_deref(), read_var(DOWNLOAD_VAR)?.as_deref())
    }

    /// A variable that is set but is not Unicode is an error, never an absent
    /// variable: treating it as absent would fall back to GitHub silently.
    fn read_var(name: &str) -> Result<Option<String>, String> {
        match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => Err(format!("{name} is not valid Unicode")),
        }
    }

    /// Both variables are needed together, so a build never reads a listing from
    /// the test server and downloads from GitHub.
    pub fn resolve(listing: Option<&str>, download: Option<&str>) -> Result<Endpoints, String> {
        match (listing, download) {
            (None, None) => Ok(Endpoints::published()),
            (Some(listing), Some(download)) => Ok(Endpoints {
                listing: loopback_url(LISTING_VAR, listing)?,
                download_base: loopback_url(DOWNLOAD_VAR, download)?,
            }),
            _ => Err(format!("{LISTING_VAR} and {DOWNLOAD_VAR} must be set together")),
        }
    }

    fn loopback_url(name: &str, value: &str) -> Result<String, String> {
        let url = reqwest::Url::parse(value).map_err(|error| format!("{name} is not a URL: {error}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!("{name} must use http or https"));
        }
        let plain = url.username().is_empty() && url.password().is_none();
        if !is_loopback(&url) || !plain || url.query().is_some() || url.fragment().is_some() {
            return Err(format!(
                "{name} must be a loopback IP address with no user, query or fragment, such as http://127.0.0.1:8080/"
            ));
        }
        Ok(value.trim_end_matches('/').to_string())
    }

    /// True for an IP literal in the loopback range. Host names are refused,
    /// because resolving one could reach anywhere.
    pub fn is_loopback(url: &reqwest::Url) -> bool {
        url.host_str()
            .map(|host| host.trim_start_matches('[').trim_end_matches(']'))
            .and_then(|host| host.parse::<IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback())
    }

    /// Whether an override client may follow a redirect to `target`, given the
    /// URLs already requested in `previous`. The target must be a loopback
    /// address. A plain HTTP target is allowed only when every earlier request
    /// was plain HTTP too, so the chain never falls back from HTTPS.
    pub fn redirect_allowed(previous: &[reqwest::Url], target: &reqwest::Url) -> bool {
        match target.scheme() {
            "https" => is_loopback(target),
            "http" => is_loopback(target) && previous.iter().all(|url| url.scheme() == "http"),
            _ => false,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn url(text: &str) -> reqwest::Url {
            reqwest::Url::parse(text).unwrap()
        }

        #[test]
        fn a_redirect_stays_on_loopback_and_never_falls_back_from_https() {
            let plain = url("http://127.0.0.1:8080/repos");
            let secure = url("https://127.0.0.1:8443/repos");
            // Plain HTTP to plain HTTP, and HTTP upgraded to HTTPS, are allowed.
            assert!(redirect_allowed(std::slice::from_ref(&plain), &url("http://127.0.0.2:9/x")));
            assert!(redirect_allowed(std::slice::from_ref(&plain), &secure));
            // HTTPS to HTTPS on loopback is allowed.
            assert!(redirect_allowed(std::slice::from_ref(&secure), &url("https://127.0.0.2:9/x")));
            // A downgrade from HTTPS to HTTP is refused, even on loopback.
            assert!(!redirect_allowed(std::slice::from_ref(&secure), &plain));
            assert!(!redirect_allowed(&[plain.clone(), secure.clone()], &url("http://[::1]:9/x")));
        }

        #[test]
        fn a_redirect_to_a_host_name_or_a_public_address_is_refused() {
            let origin = [url("http://127.0.0.1:8080/repos")];
            for target in [
                "http://example.com/repos",
                "https://api.github.com/repos/BarraDev/slashit/releases",
                "http://localhost:8080/",
                "http://10.0.0.1/",
                "https://github.com/BarraDev/slashit/releases/download/v0.0.1/latest.json",
            ] {
                assert!(!redirect_allowed(&origin, &url(target)), "{target} must be refused");
            }
        }

        #[test]
        fn a_redirect_to_another_scheme_is_refused() {
            let origin = [url("http://127.0.0.1:8080/repos")];
            assert!(!redirect_allowed(&origin, &url("ftp://127.0.0.1/x")));
        }

        #[test]
        fn loopback_ip_literals_are_accepted() {
            for value in ["http://127.0.0.1:8080/repos", "https://127.0.0.2/x", "http://[::1]:9/download/"] {
                assert!(loopback_url(LISTING_VAR, value).is_ok(), "{value} must be accepted");
            }
        }

        #[test]
        fn anything_that_could_reach_another_host_is_refused() {
            for value in [
                "http://example.com/repos",
                "https://api.github.com/repos/BarraDev/slashit/releases",
                "http://localhost:8080/",
                "http://127.0.0.1.nip.io/",
                "http://10.0.0.1/",
                "http://0.0.0.0/",
                "ftp://127.0.0.1/",
                "http://user@127.0.0.1/",
                "http://127.0.0.1/x?per_page=1",
            ] {
                assert!(loopback_url(LISTING_VAR, value).is_err(), "{value} must be refused");
            }
        }

        #[test]
        fn both_variables_are_required_together_and_neither_alone_is_an_override() {
            assert_eq!(resolve(None, None), Ok(Endpoints::published()));
            assert!(resolve(Some("http://127.0.0.1:1/"), None).is_err());
            assert!(resolve(None, Some("http://127.0.0.1:1/")).is_err());
        }

        #[test]
        fn a_plain_http_download_location_must_also_be_loopback() {
            let listing = Some("http://127.0.0.1:8080/repos/BarraDev/slashit/releases");
            assert!(resolve(listing, Some("http://example.com/download")).is_err());
            assert!(resolve(listing, Some("http://127.0.0.1:8080/download")).is_ok());
        }

        #[test]
        fn a_complete_override_replaces_both_locations_without_trailing_slashes() {
            let endpoints = resolve(
                Some("http://127.0.0.1:8080/repos/BarraDev/slashit/releases"),
                Some("http://127.0.0.1:8080/download/"),
            )
            .unwrap();
            assert_eq!(endpoints.listing, "http://127.0.0.1:8080/repos/BarraDev/slashit/releases");
            assert_eq!(endpoints.download_base, "http://127.0.0.1:8080/download");
        }
    }
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
            manifest_url(&Endpoints::published().download_base, "v0.0.1"),
            "https://github.com/BarraDev/slashit/releases/download/v0.0.1/latest.json"
        );
    }

    #[test]
    fn the_production_endpoints_are_the_published_github_locations() {
        let published = Endpoints::published();
        assert_eq!(published.listing, "https://api.github.com/repos/BarraDev/slashit/releases");
        assert_eq!(published.download_base, "https://github.com/BarraDev/slashit/releases/download");
    }

    /// A production build has no seam to read, so this holds whatever the
    /// environment says. This test does not set the variables itself; the
    /// verification run executes it with both variables set to loopback values.
    #[cfg(not(feature = "updater-test-endpoints"))]
    #[test]
    fn endpoints_ignore_the_test_variables_in_a_production_build() {
        assert_eq!(endpoints(), Ok(Endpoints::published()));
    }

    #[test]
    fn a_manifest_naming_another_version_is_refused() {
        let chosen = Chosen {
            tag: "v0.0.2".to_string(),
            version: version("0.0.2"),
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

    /// The published client never sends a plaintext request, in either build.
    /// This holds with the test seam compiled in but no override configured.
    /// If plaintext were sent, the queued 200 response would be read and the
    /// fetch would succeed, so the assertion would fail.
    #[tokio::test]
    async fn the_published_client_refuses_plain_http_before_connecting() {
        let (listing, _server) = serve(vec![response("200 OK", "", &full_page(0, 1))]);
        let error = fetch_feed(&client(&Endpoints::published()).unwrap(), &listing)
            .await
            .unwrap_err();
        assert!(matches!(error, FeedError::Unreachable(_)), "{error:?}");
    }

    /// The plugin's client makes the manifest request and the payload download.
    /// With no override it refuses a plain HTTP request before connecting, in
    /// either build, the same as the listing client.
    #[tokio::test]
    async fn the_plugin_client_refuses_plain_http_before_connecting() {
        let (plain, _server) = serve(vec![response("200 OK", "", "")]);
        let client = plugin_client_policy(&Endpoints::published())(
            reqwest::Client::builder().timeout(Duration::from_secs(5)),
        )
        .build()
        .unwrap();
        assert!(client.get(plain.as_str()).send().await.is_err(), "a plain HTTP request must not be sent");
    }

    // --- Against a local TLS server ---------------------------------------
    //
    // A redirect from HTTPS to HTTP is refused only when the origin is HTTPS,
    // so these tests need a TLS origin. Its certificate is generated per run and
    // trusted through the client builder. No key is committed.

    /// A TLS server configuration for `localhost` and `127.0.0.1`, and the
    /// certificate a client must trust to reach it.
    fn tls_identity() -> (std::sync::Arc<rustls::ServerConfig>, reqwest::Certificate) {
        install_tls_provider();
        let generated = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .unwrap();
        let certificate = generated.cert.der().clone();
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(generated.key_pair.serialize_der());
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], key.into())
            .unwrap();
        let trusted = reqwest::Certificate::from_der(certificate.as_ref()).unwrap();
        (std::sync::Arc::new(config), trusted)
    }

    /// A TLS server that answers one connection per response, in order. It
    /// returns the port it listens on.
    fn serve_tls(
        config: std::sync::Arc<rustls::ServerConfig>,
        responses: Vec<String>,
    ) -> (u16, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            for reply in responses {
                let (stream, _) = listener.accept().unwrap();
                let connection = rustls::ServerConnection::new(config.clone()).unwrap();
                let mut tls = rustls::StreamOwned::new(connection, stream);
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    match tls.read(&mut byte) {
                        Ok(1) => request.push(byte[0]),
                        _ => break,
                    }
                }
                let _ = tls.write_all(reply.as_bytes());
                tls.conn.send_close_notify();
                let _ = tls.flush();
            }
        });
        (port, handle)
    }

    /// A client that trusts only the test certificate, on every OS. A timeout
    /// turns a regression that lets a request hang into a failure.
    fn tls_plugin_client(trusted: reqwest::Certificate) -> reqwest::Client {
        plugin_client_policy(&Endpoints::published())(
            reqwest::Client::builder()
                .tls_certs_only([trusted])
                .timeout(Duration::from_secs(5)),
        )
        .build()
        .unwrap()
    }

    /// An HTTPS origin that redirects to plain HTTP is refused, and the plain
    /// HTTP server receives no connection.
    #[tokio::test]
    async fn the_plugin_client_refuses_an_https_redirect_to_plain_http() {
        let (config, trusted) = tls_identity();
        let plain = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        plain.set_nonblocking(true).unwrap();
        let plain_url = format!("http://{}/payload", plain.local_addr().unwrap());
        let (port, origin) = serve_tls(
            config,
            vec![response("302 Found", &format!("Location: {plain_url}\r\n"), "")],
        );
        let error = tls_plugin_client(trusted)
            .get(format!("https://127.0.0.1:{port}/manifest"))
            .send()
            .await
            .unwrap_err();
        origin.join().unwrap();
        assert!(error.is_redirect(), "{error:?}");
        assert!(plain.accept().is_err(), "the plain HTTP server must receive no connection");
    }

    /// A redirect from HTTPS to another HTTPS host is followed. A manifest's
    /// asset URL reaches GitHub's CDN this way.
    #[tokio::test]
    async fn the_plugin_client_follows_an_https_redirect_to_another_https_host() {
        let (config, trusted) = tls_identity();
        let (target, target_server) =
            serve_tls(config.clone(), vec![response("200 OK", "", "payload")]);
        let (origin, origin_server) = serve_tls(
            config,
            vec![response("302 Found", &format!("Location: https://localhost:{target}/payload\r\n"), "")],
        );
        let body = tls_plugin_client(trusted)
            .get(format!("https://127.0.0.1:{origin}/manifest"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        origin_server.join().unwrap();
        target_server.join().unwrap();
        assert_eq!(body, "payload");
    }

    /// In production a payload must be HTTPS, on any host. Other schemes and a
    /// value that is not a URL are refused before any download.
    #[test]
    fn a_production_payload_must_be_https() {
        let published = Endpoints::published();
        for payload in [
            "https://api.github.com/repos/BarraDev/slashit/releases/assets/1",
            "https://release-assets.githubusercontent.com/github-production-release-asset/1",
        ] {
            assert!(check_payload_location(&published, payload).is_ok(), "{payload} must be accepted");
        }
        for payload in [
            "http://api.github.com/repos/BarraDev/slashit/releases/assets/1",
            "http://127.0.0.1:8080/payload",
            "ftp://example.com/payload",
            "file:///tmp/payload",
            "not a url",
            "",
        ] {
            assert!(check_payload_location(&published, payload).is_err(), "{payload} must be refused");
        }
    }

    /// The endpoints of a test build whose listing is `listing`. The download
    /// base is a fixed loopback address on another port, which these tests do
    /// not contact.
    #[cfg(feature = "updater-test-endpoints")]
    fn override_for(listing: &str) -> Endpoints {
        Endpoints {
            listing: listing.to_string(),
            download_base: "http://127.0.0.1:9/download".to_string(),
        }
    }

    /// A test build with a configured override reads a loopback listing over
    /// plain HTTP.
    #[cfg(feature = "updater-test-endpoints")]
    #[tokio::test]
    async fn the_test_client_reads_a_loopback_listing_over_plain_http() {
        let (listing, server) = serve(vec![response("200 OK", "", &full_page(0, 2))]);
        let feed = fetch_feed(&client(&override_for(&listing)).unwrap(), &listing).await.unwrap();
        server.join().unwrap();
        assert_eq!(feed.candidates.len(), 2);
    }

    /// A test build with a configured override follows a redirect to another
    /// loopback listing.
    #[cfg(feature = "updater-test-endpoints")]
    #[tokio::test]
    async fn the_test_client_follows_a_redirect_to_a_loopback_address() {
        let (second, second_server) = serve(vec![response("200 OK", "", &full_page(0, 2))]);
        let (first, first_server) =
            serve(vec![response("302 Found", &format!("Location: {second}\r\n"), "")]);
        let feed = fetch_feed(&client(&override_for(&first)).unwrap(), &first).await.unwrap();
        first_server.join().unwrap();
        second_server.join().unwrap();
        assert_eq!(feed.candidates.len(), 2);
    }

    /// A test build with a configured override refuses a redirect to a public
    /// host before connecting to it.
    #[cfg(feature = "updater-test-endpoints")]
    #[tokio::test]
    async fn the_test_client_refuses_a_redirect_to_a_public_host() {
        let (first, first_server) = serve(vec![response(
            "302 Found",
            "Location: http://example.com/repos/BarraDev/slashit/releases\r\n",
            "",
        )]);
        // Assert on the redirect refusal itself. A followed redirect would fail
        // later with a DNS or connection error, which is not a proof of refusal.
        let error = client(&override_for(&first))
            .unwrap()
            .get(&first)
            .send()
            .await
            .unwrap_err();
        first_server.join().unwrap();
        assert!(error.is_redirect(), "{error:?}");
    }

    /// The plugin's own client follows the same rule: a manifest or payload
    /// redirect from a configured loopback override to a public host is refused.
    #[cfg(feature = "updater-test-endpoints")]
    #[tokio::test]
    async fn the_plugin_client_refuses_a_redirect_to_a_public_host() {
        let (first, first_server) = serve(vec![response(
            "302 Found",
            "Location: https://example.com/BarraDev/slashit/releases/download/v0.0.2/latest.json\r\n",
            "",
        )]);
        let builder = plugin_client_policy(&override_for(&first))(reqwest::Client::builder());
        let error = builder.build().unwrap().get(&first).send().await.unwrap_err();
        first_server.join().unwrap();
        assert!(error.is_redirect(), "{error:?}");
    }

    /// With an override, the manifest's payload must be loopback, whichever
    /// scheme it uses. A build with no override keeps the published location.
    #[cfg(feature = "updater-test-endpoints")]
    #[test]
    fn a_payload_must_come_from_the_configured_test_server_only_under_an_override() {
        let overridden = Endpoints {
            listing: "http://127.0.0.1:8080/repos/BarraDev/slashit/releases".to_string(),
            download_base: "http://127.0.0.1:8080/download".to_string(),
        };
        assert!(check_payload_location(&overridden, "http://127.0.0.1:8080/download/v0.0.2/payload").is_ok());
        for payload in [
            // The same host on another port, and an https origin on the same port, are other origins.
            "http://127.0.0.1:9999/download/v0.0.2/payload",
            "https://127.0.0.1:8080/download/v0.0.2/payload",
            "http://example.com/download/v0.0.2/payload",
            "https://github.com/BarraDev/slashit/releases/download/v0.0.2/payload",
            "http://localhost:8080/payload",
            "not a url",
        ] {
            assert!(check_payload_location(&overridden, payload).is_err(), "{payload} must be refused");
        }
        let published = Endpoints::published();
        assert!(check_payload_location(&published, "https://github.com/BarraDev/slashit/releases/download/v0.0.2/payload").is_ok());
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
