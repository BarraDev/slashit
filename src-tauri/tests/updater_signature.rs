// SPDX-License-Identifier: MPL-2.0
//! The Tauri updater's own discovery and signature verification, run against a
//! local HTTP server with a disposable key.
//!
//! The application's release-listing selection, version policy and channel
//! rules are covered by unit tests in `update_feed` and `commands::updater`.
//! This file covers the stages that belong to the plugin: fetching a static
//! manifest, comparing its version with the running build, downloading the
//! payload, and verifying the payload's signature before it is returned.
//!
//! The installation stage is deliberately not run. Nothing here calls
//! `Update::install`, so no installed file, executable or user data is touched.
//! Every fixture under `tests/fixtures/updater` was signed with a disposable key
//! whose private half was deleted after signing. See the README there.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tauri::test::{mock_builder, mock_context, noop_assets, MockRuntime};
use tauri_plugin_updater::{Builder as UpdaterPlugin, Error, Updater, UpdaterExt};

/// The platform key the manifests name. The plugin looks the artifact up by it.
const TARGET: &str = "linux-x86_64";
/// The package version the mock context reports. Asserted before any test relies on it.
const RUNNING_VERSION: &str = "0.1.0";

const PUBKEY: &str = include_str!("fixtures/updater/disposable.pub");
const PAYLOAD: &[u8] = include_bytes!("fixtures/updater/payload-2.0.0.bin");
const TAMPERED: &[u8] = include_bytes!("fixtures/updater/payload-2.0.0-tampered.bin");
const SIGNATURE: &str = include_str!("fixtures/updater/payload-2.0.0.sig");
const OTHER_KEY_SIGNATURE: &str = include_str!("fixtures/updater/payload-2.0.0-other-key.sig");

const MANIFEST: &str = "/latest.json";
const ARTIFACT: &str = "/payload-2.0.0.bin";

/// Path to (status, body). The test may change it between requests.
type Routes = Arc<Mutex<HashMap<String, (u16, Vec<u8>)>>>;

/// Binds a local HTTP/1.1 server answering from `routes`, and returns its base URL.
/// A path with no route answers 404.
fn serve(routes: Routes) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
    let base = format!("http://{}", listener.local_addr().expect("local address"));
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let routes = routes.clone();
            thread::spawn(move || answer(stream, &routes));
        }
    });
    base
}

fn answer(mut stream: TcpStream, routes: &Routes) {
    let Ok(clone) = stream.try_clone() else { return };
    let mut reader = BufReader::new(clone);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) | Err(_) => break,
            Ok(_) if header == "\r\n" => break,
            Ok(_) => {}
        }
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let (status, body) = routes
        .lock()
        .expect("routes lock")
        .get(&path)
        .cloned()
        .unwrap_or((404, b"not found".to_vec()));
    let head = format!(
        "HTTP/1.1 {status} status\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
}

fn publish(routes: &Routes, path: &str, status: u16, body: Vec<u8>) {
    routes.lock().expect("routes lock").insert(path.to_string(), (status, body));
}

/// A static updater manifest offering `version` for [`TARGET`].
fn manifest(version: &str, signature: &str, url: &str) -> Vec<u8> {
    serde_json::json!({
        "version": version,
        "notes": "integration fixture",
        "pub_date": "2026-10-26T00:00:00Z",
        "platforms": { TARGET: { "signature": signature, "url": url } }
    })
    .to_string()
    .into_bytes()
}

/// Serves a manifest offering 2.0.0 whose artifact is `artifact` and whose
/// signature is `signature`. Returns the routes and the base URL.
fn start(artifact: &[u8], signature: &str) -> (Routes, String) {
    let routes = Routes::default();
    let base = serve(routes.clone());
    publish(&routes, ARTIFACT, 200, artifact.to_vec());
    publish(&routes, MANIFEST, 200, manifest("2.0.0", signature, &format!("{base}{ARTIFACT}")));
    (routes, base)
}

/// The mock app with the real updater plugin, configured with the disposable key.
/// The mock context leaves `plugins.updater` null, which the plugin cannot read,
/// so the section is supplied the way a real `tauri.conf.json` would.
fn app() -> tauri::App<MockRuntime> {
    let mut context: tauri::Context<MockRuntime> = mock_context(noop_assets());
    context.config_mut().plugins.0.insert(
        "updater".to_string(),
        serde_json::json!({ "pubkey": PUBKEY.trim(), "endpoints": [] }),
    );
    mock_builder()
        .plugin(UpdaterPlugin::new().build())
        .build(context)
        .expect("mock app")
}

fn updater(app: &tauri::App<MockRuntime>, base: &str) -> Updater {
    app.handle()
        .updater_builder()
        .endpoints(vec![format!("{base}{MANIFEST}").parse().expect("manifest URL")])
        .expect("endpoint")
        .target(TARGET)
        .timeout(Duration::from_secs(10))
        .build()
        .expect("updater")
}

#[tokio::test]
async fn the_mock_app_reports_the_running_version_the_tests_assume() {
    assert_eq!(app().package_info().version.to_string(), RUNNING_VERSION);
}

#[tokio::test]
async fn a_newer_release_is_discovered_for_its_platform() {
    let (_routes, base) = start(PAYLOAD, SIGNATURE.trim());

    let app = app();
    let found = updater(&app, &base)
        .check()
        .await
        .expect("the manifest is readable")
        .expect("2.0.0 is newer than the running 0.1.0");

    assert_eq!(found.version, "2.0.0");
    assert_eq!(found.current_version, RUNNING_VERSION);
    assert_eq!(found.target, TARGET);
}

#[tokio::test]
async fn a_valid_artifact_and_signature_are_accepted() {
    let (_routes, base) = start(PAYLOAD, SIGNATURE.trim());

    let app = app();
    let found = updater(&app, &base).check().await.expect("manifest").expect("update");
    // Download is where the plugin verifies the payload. Installation is not run.
    let bytes = found
        .download(|_, _| {}, || {})
        .await
        .expect("a payload signed by the configured key verifies");

    assert_eq!(bytes, PAYLOAD);
}

#[tokio::test]
async fn an_artifact_changed_after_signing_is_refused() {
    // The signature is the genuine one; the bytes served are not what it signed.
    let (_routes, base) = start(TAMPERED, SIGNATURE.trim());

    let app = app();
    let found = updater(&app, &base).check().await.expect("manifest").expect("update");
    let outcome = found.download(|_, _| {}, || {}).await;

    assert!(
        matches!(outcome, Err(Error::Minisign(_))),
        "a changed artifact must fail minisign verification, got {outcome:?}"
    );
}

#[tokio::test]
async fn a_signature_from_another_key_is_refused() {
    let (_routes, base) = start(PAYLOAD, OTHER_KEY_SIGNATURE.trim());

    let app = app();
    let found = updater(&app, &base).check().await.expect("manifest").expect("update");
    let outcome = found.download(|_, _| {}, || {}).await;

    assert!(
        matches!(outcome, Err(Error::Minisign(_))),
        "a signature the configured key did not make must be refused, got {outcome:?}"
    );
}

#[tokio::test]
async fn a_release_that_is_not_newer_is_never_offered() {
    // No downgrade: an equal or older manifest reads as no update, never as an
    // offer to move backwards.
    for version in ["0.1.0", "0.0.9"] {
        let routes = Routes::default();
        let base = serve(routes.clone());
        publish(&routes, ARTIFACT, 200, PAYLOAD.to_vec());
        publish(&routes, MANIFEST, 200, manifest(version, SIGNATURE.trim(), &format!("{base}{ARTIFACT}")));

        let app = app();
        let answer = updater(&app, &base).check().await.map(|found| found.map(|update| update.version));

        assert!(
            matches!(answer, Ok(None)),
            "{version} is not newer than {RUNNING_VERSION}, got {answer:?}"
        );
    }
}

#[tokio::test]
async fn a_malformed_manifest_is_an_error_and_not_up_to_date() {
    let routes = Routes::default();
    let base = serve(routes.clone());
    publish(&routes, MANIFEST, 200, b"{ this is not a manifest".to_vec());

    let app = app();
    let answer = updater(&app, &base).check().await.map(|found| found.map(|update| update.version));

    assert!(answer.is_err(), "an unreadable manifest must not read as up to date, got {answer:?}");
}

#[tokio::test]
async fn an_unreachable_manifest_is_an_error_and_not_up_to_date() {
    // Reserve a port, then close it, so nothing listens there.
    let port = TcpListener::bind("127.0.0.1:0").expect("bind").local_addr().expect("address").port();

    let app = app();
    let answer = updater(&app, &format!("http://127.0.0.1:{port}")).check().await.map(|found| found.map(|update| update.version));

    assert!(answer.is_err(), "an unreachable manifest must not read as up to date, got {answer:?}");
}

#[tokio::test]
async fn an_update_already_found_keeps_the_release_it_was_found_as() {
    // The person accepted 2.0.0 from this `Update`. A later manifest that offers
    // 3.0.0 must not change what this value fetches or verifies.
    let (routes, base) = start(PAYLOAD, SIGNATURE.trim());

    let app = app();
    let plugin_updater = updater(&app, &base);
    let found = plugin_updater.check().await.expect("manifest").expect("update");

    publish(
        &routes,
        MANIFEST,
        200,
        manifest("3.0.0", OTHER_KEY_SIGNATURE.trim(), &format!("{base}{ARTIFACT}")),
    );

    assert_eq!(found.version, "2.0.0");
    let bytes = found
        .download(|_, _| {}, || {})
        .await
        .expect("the release already found still verifies");
    assert_eq!(bytes, PAYLOAD, "the accepted release, not the newer manifest, is what is fetched");
}
