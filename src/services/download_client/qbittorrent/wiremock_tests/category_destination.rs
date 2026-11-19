//! Category placement is selected from qBittorrent metadata, never by a
//! suffix heuristic on a caller-provided directory. No live clients involved.
use super::fixture::{install_login_ok, new_with_category};
use crate::services::download_client::qbittorrent::QbitClient;
use crate::services::download_client::{AddOutcome, DownloadClient};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const HASH: &str = "aabbccddeeff00112233445566778899aabbccdd";
const MAGNET: &str = "magnet:?xt=urn:btih:aabbccddeeff00112233445566778899aabbccdd";
const BASE: &str = "/media/anime";
const CANDIDATE: &str = "/media/anime/Overgeared/Season 01";
const ROOT: &str = "/media/anime/ryokan-anime";
const DEST: &str = "/media/anime/ryokan-anime/Overgeared/Season 01";

async fn fixture(
    category_path: Option<&str>,
    preferences: Value,
    mapped_root: &str,
) -> (MockServer, QbitClient) {
    let (server, client) = new_with_category("ryokan-anime").await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    let category = match category_path {
        Some(path) => json!({"savePath": path}),
        None => json!({}),
    };
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/categories"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ryokan-anime": category})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v2/app/preferences"))
        .respond_with(ResponseTemplate::new(200).set_body_json(preferences))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/add"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Ok."))
        .mount(&server)
        .await;
    (server, client.with_library_root(mapped_root))
}

async fn assert_add(server: &MockServer, expected: &str) {
    let requests = server.received_requests().await.unwrap();
    let adds: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path() == "/api/v2/torrents/add")
        .collect();
    assert_eq!(adds.len(), 1);
    let body = String::from_utf8_lossy(&adds[0].body);
    let pairs: Vec<_> = body
        .split('&')
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap();
            (
                key,
                urlencoding::decode(&value.replace('+', " "))
                    .unwrap()
                    .into_owned(),
            )
        })
        .collect();
    assert_eq!(pairs.iter().filter(|(k, _)| *k == "savepath").count(), 1);
    let value = |key| pairs.iter().find(|(k, _)| *k == key).unwrap().1.as_str();
    assert_eq!(value("savepath"), expected);
    assert_eq!(value("autoTMM"), "false");
    assert_eq!(value("category"), "ryokan-anime");
    assert_eq!(value("urls"), MAGNET);
}

async fn assert_no_add(server: &MockServer) {
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.url.path() != "/api/v2/torrents/add")
    );
}

#[tokio::test]
async fn explicit_category_save_path_is_authoritative_in_manual_and_auto_modes() {
    for automatic in [false, true] {
        let (server, client) = fixture(
            Some(ROOT),
            json!({"save_path": BASE, "auto_tmm_enabled": automatic}),
            BASE,
        )
        .await;
        client
            .add_torrent_at(MAGNET, HASH, Some(CANDIDATE))
            .await
            .unwrap();
        assert_add(&server, DEST).await;
    }
}

#[tokio::test]
async fn automatic_category_folders_follow_the_management_preferences() {
    for (automatic, manual_categories) in [(true, false), (false, true)] {
        let (server, client) = fixture(Some(""), json!({"save_path": BASE, "auto_tmm_enabled": automatic, "use_category_paths_in_manual_mode": manual_categories}), BASE).await;
        client
            .add_torrent_at(MAGNET, HASH, Some(CANDIDATE))
            .await
            .unwrap();
        assert_add(&server, DEST).await;
    }
}

#[tokio::test]
async fn already_prefixed_roots_and_relative_category_paths_are_not_prefixed_again() {
    for (category, default) in [(ROOT, ROOT), ("ryokan-anime", "/media/anime")] {
        let (server, client) = fixture(
            Some(category),
            json!({"save_path": default, "auto_tmm_enabled": true}),
            ROOT,
        )
        .await;
        client
            .add_torrent_at(MAGNET, HASH, Some(DEST))
            .await
            .unwrap();
        assert_add(&server, DEST).await;
    }
}

#[tokio::test]
async fn categories_outside_the_base_are_rejected() {
    for outside in ["/external/Shows", "/media/anime-other/Shows"] {
        let (server, client) = fixture(Some(outside), json!({}), BASE).await;
        let error = client
            .add_torrent_at(MAGNET, HASH, Some(CANDIDATE))
            .await
            .unwrap_err();
        assert!(error.contains("escapes mapped base"), "{error}");
        assert_no_add(&server).await;
    }
}

#[tokio::test]
async fn inconsistent_implicit_category_configuration_is_not_silently_deduplicated() {
    // qBittorrent itself would append the implicit category to this default.
    // The resolver reports that mismatch instead of rewriting its settings.
    let (server, client) = fixture(
        Some(""),
        json!({"save_path": ROOT, "auto_tmm_enabled": true}),
        ROOT,
    )
    .await;
    let error = client
        .add_torrent_at(MAGNET, HASH, Some(DEST))
        .await
        .unwrap_err();
    assert!(error.contains("ryokan-anime/ryokan-anime"), "{error}");
    assert_no_add(&server).await;
}

#[tokio::test]
async fn ambiguous_category_configuration_is_not_guessed_from_the_base() {
    for category in [Some(""), None] {
        let (server, client) = fixture(category, json!({}), BASE).await;
        let error = client
            .add_torrent_at(MAGNET, HASH, Some(CANDIDATE))
            .await
            .unwrap_err();
        assert!(error.contains("ambiguous"), "{error}");
        assert_no_add(&server).await;
    }
}

#[tokio::test]
async fn manual_mode_without_category_paths_uses_the_reported_default_directory() {
    let (server, client) = fixture(Some(""), json!({"save_path": BASE, "auto_tmm_enabled": false, "use_category_paths_in_manual_mode": false}), BASE).await;
    client
        .add_torrent_at(MAGNET, HASH, Some(CANDIDATE))
        .await
        .unwrap();
    assert_add(&server, CANDIDATE).await;
}

#[tokio::test]
async fn unsupported_configuration_api_requires_an_explicit_category_destination() {
    let server = MockServer::start().await;
    install_login_ok(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    // Unmatched categories/preferences endpoints return 404.
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/add"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Ok."))
        .mount(&server)
        .await;
    let client = QbitClient::new(&server.uri(), "u", "p", "ryokan-anime").with_library_root(ROOT);
    let error = client
        .add_torrent_at(MAGNET, HASH, Some(DEST))
        .await
        .unwrap_err();
    assert!(
        error.contains("configure an explicit category save path"),
        "{error}"
    );
    assert_no_add(&server).await;
}

#[tokio::test]
async fn existing_torrent_bypasses_changed_category_configuration() {
    let (server, client) = fixture(Some("/changed/category"), json!({}), "/old/mapping").await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "hash": HASH, "name":"existing", "size":100, "progress":1.0,
            "dlspeed":0, "state":"uploading", "category":"other", "eta":0
        }])))
        .with_priority(1)
        .mount(&server)
        .await;
    assert_eq!(
        client
            .add_torrent_at(MAGNET, HASH, Some(DEST))
            .await
            .unwrap()
            .0,
        AddOutcome::AlreadyPresent
    );
    assert_no_add(&server).await;
    assert!(
        server.received_requests().await.unwrap().iter().all(|r| !r
            .url
            .path()
            .ends_with("categories")
            && !r.url.path().ends_with("preferences"))
    );
}

#[tokio::test]
async fn different_filesystem_views_map_the_same_category_directory_to_general_media_root() {
    use crate::models::{config, download_clients};
    use crate::test_support::{in_memory_pool, seed_series};

    let local = tempfile::tempdir().unwrap();
    let local_root = local.path().join("library");
    std::fs::create_dir_all(&local_root).unwrap();
    let db = in_memory_pool().await;
    config::save_config(
        &db,
        &config::Config {
            media_root: local_root.to_str().unwrap().into(),
            post_processing_enabled: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let series_id = seed_series(&db, 77777, "Overgeared").await;
    let (server, client) = fixture(Some(ROOT), json!({"save_path": BASE}), BASE).await;
    let client_id = download_clients::insert(
        &db,
        download_clients::DownloadClientForm {
            name: "mock qbit",
            kind: "qbittorrent",
            url: &server.uri(),
            username: "",
            password: "",
            label: "ryokan-anime",
            download_path: "/unrelated/download/mount",
            enabled: true,
            is_default: true,
        },
    )
    .await
    .unwrap();
    download_clients::set_library_destination(&db, client_id, true, BASE)
        .await
        .unwrap();
    let destination = crate::services::download_client::destination::for_grab(
        &db,
        client_id,
        Some(series_id),
        "[Yameii] Overgeared - S01E01 [1080p]",
        false,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(destination, CANDIDATE);
    assert_eq!(
        crate::services::download_client::destination::to_local(
            DEST,
            BASE,
            local_root.to_str().unwrap()
        ),
        Some(
            local_root
                .join("ryokan-anime/Overgeared/Season 01")
                .to_str()
                .unwrap()
                .into()
        )
    );
    client
        .add_torrent_at(MAGNET, HASH, Some(&destination))
        .await
        .unwrap();
    assert_add(&server, DEST).await;
}

#[tokio::test]
async fn unsafe_category_configuration_never_adds_a_torrent() {
    for category in ["../outside", "/media/anime/../outside", "C:\\outside"] {
        let (server, client) =
            fixture(Some(category), json!({"save_path": "/media/anime"}), ROOT).await;
        assert!(
            client
                .add_torrent_at(MAGNET, HASH, Some(DEST))
                .await
                .is_err()
        );
        assert_no_add(&server).await;
    }
}

#[tokio::test]
async fn category_equal_to_base_and_custom_category_directories_are_preserved() {
    for (category_root, expected) in [
        (BASE, CANDIDATE),
        (
            "/media/anime/Shows",
            "/media/anime/Shows/Overgeared/Season 01",
        ),
    ] {
        let (server, client) = fixture(Some(category_root), json!({"save_path": BASE}), BASE).await;
        client
            .add_torrent_at(MAGNET, HASH, Some(CANDIDATE))
            .await
            .unwrap();
        assert_add(&server, expected).await;
    }
}

#[tokio::test]
async fn a_pinned_category_prefix_is_not_added_twice_or_reinterpreted() {
    let (server, client) = fixture(Some(ROOT), json!({}), BASE).await;
    client
        .add_torrent_at(MAGNET, HASH, Some(DEST))
        .await
        .unwrap();
    assert_add(&server, DEST).await;
    let (server, client) = fixture(Some("/media/anime/Shows"), json!({}), BASE).await;
    assert!(
        client
            .add_torrent_at(MAGNET, HASH, Some(DEST))
            .await
            .unwrap_err()
            .contains("already stored")
    );
    assert_no_add(&server).await;
}

#[tokio::test]
async fn parent_base_is_used_by_normal_interactive_and_selective_grabs() {
    for mode in 0..3 {
        let (server, client) = fixture(Some(ROOT), json!({"save_path": BASE}), BASE).await;
        Mock::given(method("GET"))
            .and(path("/api/v2/torrents/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"name":"Overgeared - 01.mkv", "size":100, "progress":0.0, "priority":1},
                {"name":"Overgeared - 02.mkv", "size":100, "progress":0.0, "priority":1}
            ])))
            .mount(&server)
            .await;
        for endpoint in ["start", "filePrio"] {
            Mock::given(method("POST"))
                .and(path(format!("/api/v2/torrents/{endpoint}")))
                .respond_with(ResponseTemplate::new(200))
                .mount(&server)
                .await;
        }
        match mode {
            0 => {
                client
                    .add_torrent_at(MAGNET, HASH, Some(CANDIDATE))
                    .await
                    .unwrap();
            }
            1 => {
                client
                    .add_torrent_paused_at(MAGNET, HASH, Some(CANDIDATE))
                    .await
                    .unwrap();
            }
            _ => {
                client
                    .add_torrent_filtered_at(MAGNET, HASH, Some(CANDIDATE), &mut |_| Some(vec![0]))
                    .await
                    .unwrap();
            }
        }
        assert_add(&server, DEST).await;
    }
}
