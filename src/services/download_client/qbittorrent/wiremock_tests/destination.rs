//! All HTTP traffic terminates at wiremock, including metadata and controls.
async fn new_fixture() -> (
    wiremock::MockServer,
    crate::services::download_client::qbittorrent::QbitClient,
) {
    let (server, client) = super::fixture::new_fixture().await;
    (server, client.with_library_root("/media"))
}
use crate::services::download_client::{AddOutcome, DownloadClient, SelectiveOutcome};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

const HASH: &str = "aabbccddeeff00112233445566778899aabbccdd";
const MAGNET: &str = "magnet:?xt=urn:btih:aabbccddeeff00112233445566778899aabbccdd";
const DEST: &str = "/media/アニメ & Friends + 100%/Season 01";

fn form(body: &[u8]) -> std::collections::HashMap<String, String> {
    String::from_utf8_lossy(body)
        .split('&')
        .map(|item| {
            let (k, v) = item.split_once('=').unwrap();
            (
                k.to_string(),
                urlencoding::decode(&v.replace('+', " "))
                    .unwrap()
                    .into_owned(),
            )
        })
        .collect()
}

async fn install(server: &wiremock::MockServer, existing: bool) {
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/categories"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ryokan-test": {"savePath": "/media"}})),
        )
        .mount(server)
        .await;
    Mock::given(method("GET")).and(path("/api/v2/app/preferences"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"save_path": "/downloads", "auto_tmm_enabled": false, "use_category_paths_in_manual_mode": false})))
        .mount(server).await;
    let torrents = if existing {
        json!([{
            "hash": HASH, "name": "Existing", "size": 1000, "progress": 0.5,
            "dlspeed": 0, "state": "downloading", "category": "other", "eta": 0,
            "save_path": "/old/location", "content_path": "/old/location/file.mkv"
        }])
    } else {
        json!([])
    };
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(torrents))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/add"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Ok."))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"name":"Show - 01.mkv", "size":100, "progress":0.0, "priority":1},
            {"name":"Show - 02.mkv", "size":100, "progress":0.0, "priority":1}
        ])))
        .mount(server)
        .await;
    for endpoint in ["start", "filePrio", "delete"] {
        Mock::given(method("POST"))
            .and(path(format!("/api/v2/torrents/{endpoint}")))
            .respond_with(ResponseTemplate::new(200))
            .mount(server)
            .await;
    }
}

#[tokio::test]
async fn library_destination_is_encoded_for_normal_paused_and_selective_adds() {
    for mode in 0..3 {
        let (server, client) = new_fixture().await;
        install(&server, false).await;
        match mode {
            0 => {
                assert_eq!(
                    client
                        .add_torrent_at(MAGNET, HASH, Some(DEST))
                        .await
                        .unwrap()
                        .0,
                    AddOutcome::Added
                );
            }
            1 => {
                assert_eq!(
                    client
                        .add_torrent_paused_at(MAGNET, HASH, Some(DEST))
                        .await
                        .unwrap(),
                    AddOutcome::Added
                );
            }
            _ => {
                assert!(
                    matches!(client.add_torrent_filtered_at(MAGNET, HASH, Some(DEST), &mut |_| Some(vec![0])).await.unwrap(), SelectiveOutcome::Filtered(ids) if ids == vec![0])
                );
            }
        }
        let requests = server.received_requests().await.unwrap();
        let adds: Vec<_> = requests
            .iter()
            .filter(|r| r.url.path() == "/api/v2/torrents/add")
            .collect();
        assert_eq!(adds.len(), 1);
        let values = form(&adds[0].body);
        assert_eq!(values.len(), 4);
        assert_eq!(values["urls"], MAGNET);
        assert_eq!(values["category"], "ryokan-test");
        assert_eq!(values["savepath"], DEST);
        assert_eq!(values["autoTMM"], "false");
    }
}

#[tokio::test]
async fn existing_library_grabs_do_not_add_relocate_or_change_priorities() {
    for mode in 0..3 {
        let (server, client) = new_fixture().await;
        install(&server, true).await;
        match mode {
            0 => {
                assert_eq!(
                    client
                        .add_torrent_at(MAGNET, HASH, Some(DEST))
                        .await
                        .unwrap()
                        .0,
                    AddOutcome::AlreadyPresent
                );
            }
            1 => {
                assert_eq!(
                    client
                        .add_torrent_paused_at(MAGNET, HASH, Some(DEST))
                        .await
                        .unwrap(),
                    AddOutcome::AlreadyPresent
                );
            }
            _ => {
                client
                    .add_torrent_filtered_at(MAGNET, HASH, Some(DEST), &mut |_| {
                        panic!("existing torrent must stay untouched")
                    })
                    .await
                    .unwrap();
            }
        }
        for r in server.received_requests().await.unwrap() {
            assert!(
                r.method.as_str() == "GET" || r.url.path() == "/api/v2/auth/login",
                "unexpected mutation: {}",
                r.url
            );
        }
    }
}

#[tokio::test]
async fn missing_or_hashless_destination_uses_original_form_without_configuration_queries() {
    for (hash, destination) in [(HASH, None), ("", Some(DEST))] {
        let (server, client) = new_fixture().await;
        install(&server, false).await;
        client
            .add_torrent_at(MAGNET, hash, destination)
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        let add = requests
            .iter()
            .find(|r| r.url.path() == "/api/v2/torrents/add")
            .unwrap();
        assert_eq!(form(&add.body).len(), 2);
        assert!(requests.iter().all(
            |r| !r.url.path().ends_with("categories") && !r.url.path().ends_with("preferences")
        ));
    }
}

#[tokio::test]
async fn invalid_direct_destination_reports_configuration_error_without_adding() {
    for destination in [
        "relative/path",
        "/media/../escape",
        "/other/Show/Season 01",
        "/media/Season 01",
    ] {
        let (server, client) = new_fixture().await;
        install(&server, false).await;
        assert!(
            client
                .add_torrent_at(MAGNET, HASH, Some(destination))
                .await
                .is_err()
        );
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|r| r.url.path() != "/api/v2/torrents/add")
        );
    }
}

#[tokio::test]
async fn destination_probe_failure_never_posts_add_options() {
    let (server, client) = new_fixture().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    assert!(
        client
            .add_torrent_at(MAGNET, HASH, Some(DEST))
            .await
            .is_err()
    );
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
async fn deleting_a_library_torrent_preserves_data() {
    let (server, client) = new_fixture().await;
    install(&server, true).await;
    let client = client.with_library_root("/old");
    client.delete(HASH, true).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let delete = requests
        .iter()
        .find(|r| r.url.path() == "/api/v2/torrents/delete")
        .unwrap();
    assert_eq!(form(&delete.body)["deleteFiles"], "false");
}
