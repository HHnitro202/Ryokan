//! Pin imports to the torrent's actual location, never today's category settings.
use crate::models::{config::Config, series};
use crate::services::{download_client::destination::safe_absolute_path, media};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct Location {
    local_base: String,
    remote_base: String,
    remote_save_path: String,
    pub local_save_path: String,
}

impl Location {
    fn category_subdir(&self) -> Result<String, String> {
        let base = safe_absolute_path(&self.local_base).ok_or("unsafe frozen local base")?;
        let save = safe_absolute_path(&self.local_save_path).ok_or("unsafe frozen save path")?;
        let category = save
            .parent()
            .and_then(Path::parent)
            .ok_or("missing series/season directory")?;
        let subdir = category
            .strip_prefix(base)
            .map_err(|_| "direct category directory escapes General Media Root")?;
        let subdir = subdir
            .to_str()
            .ok_or("invalid category directory encoding")?;
        if !subdir.is_empty() && !media::usable_library_folder(subdir) {
            return Err("unsafe category subdirectory".into());
        }
        Ok(subdir.into())
    }
}

/// Freeze the base mapping on first observation. Later base/location changes
/// require an explicit migration, instead of silently importing or deleting
/// another path. A category change alone never affects this saved location.
pub(super) async fn resolve(
    db: &SqlitePool,
    cfg: &Config,
    grab_id: i64,
    series_id: i64,
    remote_base: &str,
    remote_save: &str,
    local_save: &str,
) -> Result<Option<Location>, String> {
    let saved: String =
        sqlx::query_scalar("SELECT direct_library_location FROM grabbed_torrents WHERE id = ?")
            .bind(grab_id)
            .fetch_one(db)
            .await
            .map_err(|e| e.to_string())?;
    let frozen = !saved.is_empty();
    if !frozen
        && crate::services::download_client::destination::to_local(
            remote_save,
            remote_base,
            &cfg.media_root,
        )
        .as_deref()
            != Some(local_save)
    {
        return Ok(None);
    }
    let location = if frozen {
        serde_json::from_str::<Location>(&saved)
            .map_err(|e| format!("invalid frozen direct-library location: {e}"))?
    } else {
        if remote_base.is_empty()
            || safe_absolute_path(remote_save).is_none()
            || safe_absolute_path(local_save).is_none()
        {
            return Ok(None);
        }
        Location {
            local_base: cfg.media_root.clone(),
            remote_base: remote_base.into(),
            remote_save_path: remote_save.into(),
            local_save_path: local_save.into(),
        }
    };
    if Path::new(&location.local_base) != Path::new(&cfg.media_root)
        || Path::new(&location.remote_base) != Path::new(remote_base)
        || Path::new(&location.remote_save_path) != Path::new(remote_save)
    {
        return Err("direct-library base mapping or torrent save path changed; refusing to reinterpret its frozen location".into());
    }
    let row = series::get_by_id(db, series_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("direct-library series no longer exists")?;
    let local_series = Path::new(&location.local_save_path)
        .parent()
        .ok_or("missing direct-library series directory")?;
    if local_series.file_name().and_then(|n| n.to_str()) != Some(row.folder_name.as_str()) {
        return if frozen {
            Err("series folder changed since this direct download was recorded; refusing to relocate it".into())
        } else {
            Ok(None)
        };
    }
    let subdir = location.category_subdir()?;
    if !row.library_subdir.is_empty() && row.library_subdir != subdir {
        return Err("direct download belongs to another category directory than the series' pinned library location; refusing to move it".into());
    }
    let local_base = PathBuf::from(&location.local_base);
    let target = PathBuf::from(&location.local_save_path);
    let old_series = local_base.join(&row.folder_name);
    let needs_binding = row.library_subdir.is_empty() && !subdir.is_empty();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        let base = local_base.canonicalize().map_err(|e| format!("direct-library base {local_base:?}: {e}"))?;
        let actual = target.canonicalize().map_err(|e| format!("direct-library season {target:?}: {e}"))?;
        if !actual.starts_with(&base) { return Err("direct-library season resolves outside General Media Root".into()); }
        if needs_binding && old_series.is_dir() {
            let mut entries = std::fs::read_dir(&old_series).map_err(|e| e.to_string())?;
            if entries.next().is_some() {
                return Err(format!("series already has data at {old_series:?}; explicit migration is required before changing its library category"));
            }
        }
        Ok(())
    }).await.map_err(|e| e.to_string())??;
    series::bind_library_subdir(db, series_id, &subdir).await?;
    if !frozen {
        let encoded = serde_json::to_string(&location).map_err(|e| e.to_string())?;
        sqlx::query("UPDATE grabbed_torrents SET direct_library_location = ? WHERE id = ? AND direct_library_location = ''")
            .bind(encoded).bind(grab_id).execute(db).await.map_err(|e| e.to_string())?;
    }
    Ok(Some(location))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{in_memory_pool, seed_grabbed_torrent, seed_series};

    struct Fixture {
        db: SqlitePool,
        cfg: Config,
        series: i64,
        grab: i64,
        save: String,
        _root: tempfile::TempDir,
    }

    impl Fixture {
        async fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let save = root.path().join("Shows/Overgeared/Season 01");
            std::fs::create_dir_all(&save).unwrap();
            let db = in_memory_pool().await;
            let series = seed_series(&db, 123, "Overgeared").await;
            let grab = seed_grabbed_torrent(&db, series, "hash", "Overgeared - 01", &[1]).await;
            let cfg = Config {
                media_root: root.path().to_str().unwrap().into(),
                ..Default::default()
            };
            Self {
                db,
                cfg,
                series,
                grab,
                save: save.to_str().unwrap().into(),
                _root: root,
            }
        }

        async fn resolve(
            &self,
            remote_base: &str,
            remote_save: &str,
        ) -> Result<Option<Location>, String> {
            resolve(
                &self.db,
                &self.cfg,
                self.grab,
                self.series,
                remote_base,
                remote_save,
                &self.save,
            )
            .await
        }
    }

    #[tokio::test]
    async fn existing_series_data_is_not_hidden_by_a_new_category() {
        let fixture = Fixture::new().await;
        let legacy = Path::new(&fixture.cfg.media_root).join("Overgeared");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("existing.mkv"), b"keep").unwrap();
        let error = fixture
            .resolve("/remote", "/remote/Shows/Overgeared/Season 01")
            .await
            .unwrap_err();
        assert!(error.contains("explicit migration"), "{error}");
        assert_eq!(std::fs::read(legacy.join("existing.mkv")).unwrap(), b"keep");
        let row = series::get_by_id(&fixture.db, fixture.series)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.library_subdir, "");
    }

    #[tokio::test]
    async fn frozen_mapping_and_reported_location_cannot_be_reinterpreted() {
        let fixture = Fixture::new().await;
        fixture
            .resolve("/remote", "/remote/Shows/Overgeared/Season 01")
            .await
            .unwrap()
            .unwrap();
        for (base, save) in [
            ("/changed", "/changed/Shows/Overgeared/Season 01"),
            ("/remote", "/remote/Other/Overgeared/Season 01"),
        ] {
            let error = fixture.resolve(base, save).await.unwrap_err();
            assert!(error.contains("frozen location"), "{error}");
        }
        assert_eq!(
            series::get_by_id(&fixture.db, fixture.series)
                .await
                .unwrap()
                .unwrap()
                .library_subdir,
            "Shows"
        );
        assert!(Path::new(&fixture.save).is_dir());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn category_symlink_outside_the_local_base_is_rejected() {
        let fixture = Fixture::new().await;
        std::fs::remove_dir_all(Path::new(&fixture.cfg.media_root).join("Shows")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(outside.path().join("Overgeared/Season 01")).unwrap();
        std::os::unix::fs::symlink(
            outside.path(),
            Path::new(&fixture.cfg.media_root).join("Shows"),
        )
        .unwrap();
        let error = fixture
            .resolve("/remote", "/remote/Shows/Overgeared/Season 01")
            .await
            .unwrap_err();
        assert!(error.contains("outside General Media Root"), "{error}");
    }

    #[tokio::test]
    async fn ordinary_downloads_outside_the_mapping_keep_the_legacy_import_path() {
        let fixture = Fixture::new().await;
        assert!(
            fixture
                .resolve("/remote", "/downloads/Overgeared")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            series::get_by_id(&fixture.db, fixture.series)
                .await
                .unwrap()
                .unwrap()
                .library_subdir,
            ""
        );
        for unsafe_prefix in [
            "../escape",
            "/absolute",
            "Shows/../../escape",
            "Shows\\Other",
        ] {
            assert!(
                series::bind_library_subdir(&fixture.db, fixture.series, unsafe_prefix)
                    .await
                    .is_err()
            );
        }
    }
}
