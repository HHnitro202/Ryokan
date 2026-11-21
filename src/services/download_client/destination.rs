//! Optional qBittorrent library destinations and the reverse mount mapping.
use crate::models::{config, download_clients, series};
use crate::services::{library_destination, numbering, release_title, rss};
use sqlx::SqlitePool;
use std::path::{Component, Path};

/// Never send a relative path, traversal, platform-ambiguous separator or
/// control character to a remote client. Spaces, Unicode, &, + and % are
/// ordinary path characters; reqwest form-encodes them.
pub fn safe_absolute_path(raw: &str) -> Option<&Path> {
    let path = Path::new(raw);
    (path.is_absolute()
        && !raw.contains('\\')
        && !raw.chars().any(char::is_control)
        && !path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir)))
    .then_some(path)
}

/// A batch needs a bounded episode claim and an exact series identity. An
/// unnumbered complete franchise or a multi-season pack cannot establish a
/// single destination, even when a later picker might narrow its files.
async fn unambiguous(db: &SqlitePool, row: &series::Series, title: &str, batch: bool) -> bool {
    if title.trim().is_empty() {
        return false;
    }
    let parsed = release_title::parse(title);
    if parsed.seasons.numbers.len() > 1
        || parsed.special
        || parsed.extra
        || parsed.includes_specials
        || parsed.name_zones().names.len() > 1
    {
        return false;
    }
    let Some(name) = parsed.series_title.as_deref() else {
        return false;
    };
    let matches_series = [
        row.title.as_str(),
        row.title_romaji.as_str(),
        row.title_english.as_str(),
        row.title_native.as_str(),
    ]
    .into_iter()
    .chain(row.alternate_titles.lines())
    .any(|alias| !alias.is_empty() && name.eq_ignore_ascii_case(alias));
    let tagged_season = numbering::season_tag(&parsed).is_some();
    if !matches_series && !tagged_season {
        return false;
    }
    if batch || parsed.is_batch() {
        let Some(claim) = parsed.episodes.as_ref() else {
            return false;
        };
        let Some(count) = row.episodes.filter(|n| *n > 0) else {
            return false;
        };
        if claim.first < 1 || claim.last > count {
            return false;
        }
    }
    if tagged_season {
        // A tagged release can use the franchise's base name while its
        // library entry includes "Season 2". Reuse metadata-aware alias,
        // season and Numbering checks; never erase suffixes to invent an
        // alias or choose arbitrarily between competing library entries.
        return rss::match_library_title_exact(db, title, batch || parsed.is_batch())
            .await
            .is_some_and(|(matched, _)| matched.id == row.id);
    }
    matches_series
}

/// Ineligible grabs keep the client's usual behavior. Eligible direct grabs
/// must carry an explicit filesystem mapping; qBittorrent validates it against
/// its category configuration at add time, after checking for an existing hash.
pub async fn for_grab(
    db: &SqlitePool,
    client_id: i64,
    series_id: Option<i64>,
    title: &str,
    batch: bool,
) -> Result<Option<String>, String> {
    let Some(client) = download_clients::get_by_id(db, client_id)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    if client.kind != "qbittorrent" || !client.direct_to_library {
        return Ok(None);
    }
    let Some(series_id) = series_id else {
        return Ok(None);
    };
    let cfg = config::get_config(db)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("Direct-library configuration: General settings are missing")?;
    if !cfg.post_processing_enabled {
        return Ok(None);
    }
    let Some(row) = series::get_by_id(db, series_id)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    if !unambiguous(db, &row, title, batch).await {
        return Ok(None);
    }
    let remote_root = safe_absolute_path(&client.library_root).ok_or(
        "Direct-library configuration: set an absolute Library root mapping for General Media Root",
    )?;
    let local_root = safe_absolute_path(&cfg.media_root)
        .ok_or("Direct-library configuration: General Media Root must be an absolute safe path")?;
    let (_, local) = library_destination::resolve(db, &cfg, &row).await?;
    let relative = local.strip_prefix(local_root).map_err(|e| e.to_string())?;
    let root = local_root.to_path_buf();
    let target = local.clone();
    let contained = tokio::task::spawn_blocking(move || {
        let Ok(root) = root.canonicalize() else {
            return false;
        };
        let Some(ancestor) = target.ancestors().find(|p| p.exists()) else {
            return false;
        };
        ancestor.canonicalize().is_ok_and(|p| p.starts_with(root))
    })
    .await
    .map_err(|e| e.to_string())?;
    if !contained {
        return Err(format!(
            "Direct-library configuration: destination {local:?} escapes General Media Root or the library is inaccessible"
        ));
    }
    let remote = remote_root.join(relative);
    let remote = remote
        .to_str()
        .ok_or("Direct-library destination is not valid UTF-8")?;
    safe_absolute_path(remote).ok_or("Direct-library destination is unsafe")?;
    tracing::debug!(client_id, series_id = row.id, media_root = %cfg.media_root,
        library_root = %client.library_root, local_destination = %local.display(), savepath = remote,
        "Prepared direct-library destination; category directory checked at add time");
    Ok(Some(remote.to_string()))
}

/// Map library downloads before applying the legacy save-path replacement.
/// Kept active when the opt-in is switched off so earlier grabs still import.
pub fn to_local(path: &str, remote_root: &str, local_root: &str) -> Option<String> {
    let path = safe_absolute_path(path)?;
    let remote = safe_absolute_path(remote_root)?;
    let local = safe_absolute_path(local_root)?;
    let relative = path.strip_prefix(remote).ok()?;
    Some(local.join(relative).to_str()?.to_string())
}

/// Recognize an already-landed video without following a symlink out of the
/// season directory. This also handles the torrent's own nested folder.
pub async fn already_in_season(source: &Path, season: &Path) -> bool {
    let source = source.to_path_buf();
    let season = season.to_path_buf();
    tokio::task::spawn_blocking(
        move || match (source.canonicalize(), season.canonicalize()) {
            (Ok(source), Ok(season)) => source.starts_with(season),
            _ => false,
        },
    )
    .await
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::download_clients::{DownloadClientForm, insert, set_library_destination};
    use crate::test_support::{in_memory_pool, seed_series};

    #[test]
    fn mapping_checks_components_and_keeps_special_characters() {
        assert_eq!(
            to_local("/remote/A & アニメ + 100%/S01", "/remote/", "/local"),
            Some("/local/A & アニメ + 100%/S01".into())
        );
        assert!(to_local("/remote-other/Show", "/remote", "/local").is_none());
        for bad in [
            "relative",
            "/remote/../escape",
            "/remote/Show\n",
            "C:\\remote\\Show",
            "/remote/Show\\..\\escape",
        ] {
            assert!(safe_absolute_path(bad).is_none(), "{bad:?}");
        }
    }

    async fn fixture(root: &Path) -> (SqlitePool, i64, i64) {
        let db = in_memory_pool().await;
        let cfg = config::Config {
            media_root: root.to_str().unwrap().into(),
            post_processing_enabled: true,
            series_folder_format: "{series.title} ({series.year})".into(),
            season_folder_format: "S{season.number:00}".into(),
            ..Default::default()
        };
        config::save_config(&db, &cfg).await.unwrap();
        let sid = seed_series(&db, 12345, "アニメ & Friends").await;
        sqlx::query(
            "UPDATE series SET folder_name = '', season_year = 2024, episodes = 12 WHERE id = ?",
        )
        .bind(sid)
        .execute(&db)
        .await
        .unwrap();
        let cid = insert(
            &db,
            DownloadClientForm {
                name: "qbit",
                kind: "qbittorrent",
                url: "http://mock.invalid",
                username: "",
                password: "",
                label: "anime",
                download_path: "/downloads",
                enabled: true,
                is_default: true,
            },
        )
        .await
        .unwrap();
        (db, cid, sid)
    }

    #[tokio::test]
    async fn destination_reuses_general_templates_and_persisted_folder() {
        let root = tempfile::tempdir().unwrap();
        let (db, cid, sid) = fixture(root.path()).await;
        let title = "[Group] アニメ & Friends - 01 [1080p]";
        assert!(
            for_grab(&db, cid, Some(sid), title, false)
                .await
                .unwrap()
                .is_none()
        );
        set_library_destination(&db, cid, true, "/qbit/anime")
            .await
            .unwrap();
        let expected = "/qbit/anime/アニメ & Friends (2024)/S01";
        assert_eq!(
            for_grab(&db, cid, Some(sid), title, false)
                .await
                .unwrap()
                .as_deref(),
            Some(expected)
        );
        sqlx::query("UPDATE config SET series_folder_format = '{series.year}'")
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(
            for_grab(&db, cid, Some(sid), title, false)
                .await
                .unwrap()
                .as_deref(),
            Some(expected)
        );
        let row = series::get_by_id(&db, sid).await.unwrap().unwrap();
        let cfg = config::get_config(&db).await.unwrap().unwrap();
        let (_, local) = library_destination::resolve(&db, &cfg, &row).await.unwrap();
        assert_eq!(
            to_local(expected, "/qbit/anime", cfg.media_root.as_str()),
            Some(local.to_str().unwrap().into())
        );
    }

    #[tokio::test]
    async fn ambiguous_grabs_fall_back_but_eligible_invalid_mappings_fail() {
        let root = tempfile::tempdir().unwrap();
        let (db, cid, sid) = fixture(root.path()).await;
        set_library_destination(&db, cid, true, "/qbit/anime")
            .await
            .unwrap();
        for (series_id, title, batch) in [
            (None, "Show - 01", false),
            (Some(sid), "アニメ & Friends S01-S03 Complete", true),
            (Some(sid), "アニメ & Friends Complete", true),
            (Some(sid), "Other Show - 01", false),
            (Some(sid), "", false),
        ] {
            assert!(
                for_grab(&db, cid, series_id, title, batch)
                    .await
                    .unwrap()
                    .is_none(),
                "{title}"
            );
        }
        set_library_destination(&db, cid, true, "../library")
            .await
            .unwrap();
        assert!(
            for_grab(&db, cid, Some(sid), "アニメ & Friends - 01", false)
                .await
                .is_err()
        );
    }

    const RETURNERS_BASE: &str = "A Returner's Magic Should Be Special";

    async fn sequel_fixture(root: &Path) -> (SqlitePool, i64, i64) {
        let (db, cid, sid) = fixture(root).await;
        let name = format!("{RETURNERS_BASE} Season 2");
        sqlx::query("UPDATE series SET title = ?, title_romaji = ?, title_english = ?, cumulative_prior_episodes = 12 WHERE id = ?")
            .bind(&name).bind(&name).bind(&name).bind(sid).execute(&db).await.unwrap();
        let parent = seed_series(&db, 7800001, RETURNERS_BASE).await;
        sqlx::query("UPDATE series SET episodes = 12 WHERE id = ?")
            .bind(parent)
            .execute(&db)
            .await
            .unwrap();
        set_library_destination(&db, cid, true, "/qbit/anime")
            .await
            .unwrap();
        (db, cid, sid)
    }

    #[tokio::test]
    async fn yameii_season_tags_resolve_to_the_tracked_sequel() {
        let root = tempfile::tempdir().unwrap();
        let (db, cid, sid) = sequel_fixture(root.path()).await;
        for episode in 1..=7 {
            let title = format!(
                "[Yameii] {RETURNERS_BASE} - S02E{episode:02} [English Dub] [1080p] [WEB-DL].mkv"
            );
            let parsed = release_title::parse(&title);
            assert_eq!(
                parsed.series_title.as_deref(),
                Some(RETURNERS_BASE),
                "{parsed:?}"
            );
            assert_eq!(parsed.season_hint(), Some(2));
            assert!(
                !parsed.special,
                "Special is part of the show's name: {parsed:?}"
            );
            assert_eq!(
                for_grab(&db, cid, Some(sid), &title, false)
                    .await
                    .unwrap()
                    .as_deref(),
                Some("/qbit/anime/A Returner's Magic Should Be Special Season 2 (2024)/S01"),
                "{title}"
            );
        }
    }

    #[tokio::test]
    async fn sequel_destinations_reject_wrong_names_seasons_and_ambiguous_packs() {
        let root = tempfile::tempdir().unwrap();
        let (db, cid, sid) = sequel_fixture(root.path()).await;
        for (release, batch) in [
            ("Unrelated Show - S02E01", false),
            ("A Returner's Magic Should Be Ordinary - S02E01", false),
            (
                "A Returner's Magic Should Be Special Side Story - S02E01",
                false,
            ),
            ("A Returner's Magic Should Be Special - S01E01", false),
            ("A Returner's Magic Should Be Special - S03E01", false),
            ("A Returner's Magic Should Be Special - 01", false),
            (
                "A Returner's Magic Should Be Special Season 2 Complete",
                true,
            ),
            ("A Returner's Magic Should Be Special S01-S03 - 01-12", true),
            ("A Returner's Magic Should Be Special - S02E01-E13", true),
            ("A Returner's Magic Should Be Special - S02E01 [PV]", false),
            (
                "A Returner's Magic Should Be Special / Unrelated Show - S02E01",
                false,
            ),
        ] {
            let title = format!("[Yameii] {release} [English Dub] [1080p]");
            assert!(
                for_grab(&db, cid, Some(sid), &title, batch)
                    .await
                    .unwrap()
                    .is_none(),
                "{title}"
            );
        }
        // The base-name alias must not let the first season take S02.
        let parent = series::get_by_anilist_id(&db, 7800001)
            .await
            .unwrap()
            .unwrap();
        let title = format!("[Yameii] {RETURNERS_BASE} - S02E01 [English Dub] [1080p]");
        assert!(
            for_grab(&db, cid, Some(parent.id), &title, false)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn tagged_sequel_batches_still_require_a_known_bounded_episode_claim() {
        let root = tempfile::tempdir().unwrap();
        let (db, cid, sid) = sequel_fixture(root.path()).await;
        let title = format!("[Yameii] {RETURNERS_BASE} - S02E01-E12 [English Dub] [1080p]");
        assert!(
            for_grab(&db, cid, Some(sid), &title, true)
                .await
                .unwrap()
                .is_some()
        );
        sqlx::query("UPDATE series SET episodes = NULL WHERE id = ?")
            .bind(sid)
            .execute(&db)
            .await
            .unwrap();
        assert!(
            for_grab(&db, cid, Some(sid), &title, true)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn competing_sequel_entries_do_not_use_an_oldest_row_tiebreak() {
        let root = tempfile::tempdir().unwrap();
        let (db, cid, sid) = sequel_fixture(root.path()).await;
        let other = seed_series(&db, 7800002, &format!("{RETURNERS_BASE} Season 2 Part 2")).await;
        sqlx::query("UPDATE series SET episodes = 12 WHERE id = ?")
            .bind(other)
            .execute(&db)
            .await
            .unwrap();
        let title = format!("[Yameii] {RETURNERS_BASE} - S02E01 [English Dub] [1080p]");
        for target in [sid, other] {
            assert!(
                for_grab(&db, cid, Some(target), &title, false)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn tagged_sequel_matching_obeys_split_numbering_and_user_overrides() {
        let root = tempfile::tempdir().unwrap();
        let (db, cid, sid) = sequel_fixture(root.path()).await;
        sqlx::query("UPDATE series SET cumulative_offset_source = 'split', cumulative_offset_season = 2 WHERE id = ?")
            .bind(sid).execute(&db).await.unwrap();
        let earlier_part = format!("[Yameii] {RETURNERS_BASE} - S02E01 [English Dub] [1080p]");
        let this_part = format!("[Yameii] {RETURNERS_BASE} - S02E13 [English Dub] [1080p]");
        assert!(
            for_grab(&db, cid, Some(sid), &earlier_part, false)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            for_grab(&db, cid, Some(sid), &this_part, false)
                .await
                .unwrap()
                .is_some()
        );
        sqlx::query("UPDATE series SET episode_numbering = 'anilist' WHERE id = ?")
            .bind(sid)
            .execute(&db)
            .await
            .unwrap();
        assert!(
            for_grab(&db, cid, Some(sid), &earlier_part, false)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            for_grab(&db, cid, Some(sid), &this_part, false)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_series_cannot_escape_the_library() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let (db, cid, sid) = fixture(root.path()).await;
        sqlx::query("UPDATE series SET folder_name = 'escape' WHERE id = ?")
            .bind(sid)
            .execute(&db)
            .await
            .unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        set_library_destination(&db, cid, true, "/qbit/anime")
            .await
            .unwrap();
        assert!(
            for_grab(&db, cid, Some(sid), "アニメ & Friends - 01", false)
                .await
                .is_err()
        );
    }
}
