//! Resolve qBittorrent's category directory before adding Ryokan's folders.
//! Absolute/relative category save paths are explicit. An implicit category
//! folder is used only when the reported management settings select it.
//! Per-torrent autoTMM=false means later Auto-TMM relocation triggers do not
//! apply to these adds. Category path semantics are documented upstream:
//! https://github.com/qbittorrent/qBittorrent/blob/release-5.1.4/src/base/bittorrent/sessionimpl.cpp
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::QbitClient;
use crate::services::download_client::destination::safe_absolute_path;

type Categories = HashMap<String, Category>;

#[derive(Deserialize)]
struct Category {
    #[serde(rename = "savePath")]
    save_path: Option<String>,
}

#[derive(Deserialize)]
struct Preferences {
    save_path: Option<String>,
    auto_tmm_enabled: Option<bool>,
    use_category_paths_in_manual_mode: Option<bool>,
}

fn absolute(raw: &str) -> Result<PathBuf, String> {
    safe_absolute_path(raw)
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            format!("qbit direct-library configuration: unsafe or non-absolute directory {raw:?}")
        })
}

fn category_root(
    category: &str,
    categories: Option<&Categories>,
    preferences: Option<&Preferences>,
) -> Result<Option<PathBuf>, String> {
    if category.is_empty() {
        return preferences
            .and_then(|p| p.save_path.as_deref())
            .map(absolute)
            .transpose();
    }
    let Some(entry) = categories.and_then(|c| c.get(category)) else {
        return Ok(None);
    };
    let Some(path) = entry.save_path.as_deref() else {
        return Ok(None);
    };
    if Path::new(path).is_absolute() {
        return absolute(path).map(Some);
    }
    if !path.is_empty() {
        // A configured relative save path is relative to the global save
        // directory. Never add the category label after that explicit path.
        if path.contains('\\')
            || path.chars().any(char::is_control)
            || !Path::new(path)
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
        {
            return Err(format!(
                "qbit direct-library configuration: unsafe category save path {path:?}"
            ));
        }
        return preferences
            .and_then(|p| p.save_path.as_deref())
            .map(|base| absolute(base).map(|base| base.join(path)))
            .transpose();
    }
    let Some(preferences) = preferences else {
        return Ok(None);
    };
    let uses_category = preferences.auto_tmm_enabled == Some(true)
        || preferences.use_category_paths_in_manual_mode == Some(true);
    if !uses_category {
        return if preferences.auto_tmm_enabled == Some(false)
            && preferences.use_category_paths_in_manual_mode == Some(false)
        {
            preferences.save_path.as_deref().map(absolute).transpose()
        } else {
            Ok(None)
        };
    }
    // Nested categories inherit differently across qBittorrent versions.
    // Labels needing qBittorrent's filename sanitization are ambiguous too;
    // require the explicit mapping rather than inventing their directory.
    if category.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|'])
        || category.chars().any(char::is_control)
        || !Path::new(category)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
    {
        return Ok(None);
    }
    preferences.save_path.as_deref().map(|base| {
        let base = absolute(base)?;
        let category_path = base.join(category);
        if base.file_name().and_then(|part| part.to_str()) == Some(category) {
            return Err(format!("implicit category directory {category_path:?} duplicates an already-prefixed default; configure an explicit category save path"));
        }
        Ok(category_path)
    }).transpose()
}

impl QbitClient {
    async fn configuration<T: DeserializeOwned>(
        &self,
        endpoint: &str,
    ) -> Result<Option<T>, String> {
        let response = self.do_get(endpoint).await?;
        if matches!(response.status().as_u16(), 404 | 501) {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(format!(
                "qbit direct-library configuration: {endpoint} returned HTTP {}",
                response.status()
            ));
        }
        response.json().await.map(Some).map_err(|e| {
            format!(
                "qbit direct-library configuration: invalid response from {endpoint}: {}",
                e.without_url()
            )
        })
    }

    /// Called only for a new, eligible direct grab, after duplicate detection.
    /// The mapping asserts that this remote directory and General Media Root
    /// are the same actual library in the two applications' filesystems.
    pub(super) async fn resolve_library_destination(
        &self,
        destination: &str,
    ) -> Result<String, String> {
        let mapped_root = absolute(&self.library_root).map_err(|_| {
            "qbit direct-library configuration: set an absolute Library root mapping for General Media Root".to_string()
        })?;
        let destination = absolute(destination)?;
        let relative = destination.strip_prefix(&mapped_root).map_err(|_| {
            format!("qbit direct-library destination {destination:?} is outside Library root mapping {mapped_root:?}")
        })?;
        let parts: Vec<_> = relative.components().collect();
        if parts.len() < 2 || !parts.iter().all(|c| matches!(c, Component::Normal(_))) {
            return Err("qbit direct-library destination must contain series and season below the mapped base".into());
        }
        let folders: PathBuf = parts[parts.len() - 2..].iter().collect();
        let pinned_prefix: PathBuf = parts[..parts.len() - 2].iter().collect();
        let categories: Option<Categories> =
            self.configuration("/api/v2/torrents/categories").await?;
        let preferences: Option<Preferences> =
            self.configuration("/api/v2/app/preferences").await?;
        let resolved = category_root(&self.category, categories.as_ref(), preferences.as_ref())?;
        let root = resolved.ok_or("qbit category destination is ambiguous; configure an explicit category save path or applicable torrent-management preferences; Library Root Mapping maps the base only")?;
        let category_prefix = root.strip_prefix(&mapped_root).map_err(|_| {
            format!("qbit category destination {root:?} escapes mapped base {mapped_root:?}; external category-root mappings are not supported")
        })?;
        if !pinned_prefix.as_os_str().is_empty() && pinned_prefix != category_prefix {
            return Err(format!(
                "series is already stored under {pinned_prefix:?}; refusing to reinterpret it under category directory {category_prefix:?}"
            ));
        }
        root.join(folders)
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| "qbit direct-library destination is not valid UTF-8".into())
    }
}
