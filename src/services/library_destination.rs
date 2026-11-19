//! The library folder resolver shared by grabs and imports.
use crate::models::{config, series};
use crate::services::{media, naming, nfo};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};

pub async fn resolve(
    db: &SqlitePool,
    cfg: &config::Config,
    series: &series::Series,
) -> Result<(String, PathBuf), String> {
    // Auto-generate folder_name from the series-folder template (#124)
    // if it was never set.
    // Empty (never set) or unusable (`.`, `..`, from a title written
    // before `sanitize_folder_name` handled dots and spaces together):
    // regenerate it from the template, which can't produce either.
    let folder_name = if !media::usable_folder_name(&series.folder_name) {
        if nfo::best_title(series).trim().is_empty() {
            return Err(format!(
                "series '{}' has no usable title for folder name",
                series.title
            ));
        }
        let generated = naming::series_folder(
            &cfg.series_folder_format,
            &cfg.title_language,
            &naming::SeriesNames::from_series(series),
        );
        // The uniqueness rule a new row gets (`series::upsert`): two
        // dots-only titles both render `Unknown Series`, and a remake
        // shares its original's title, so the plain name can be another
        // series' folder.
        let generated =
            series::unique_series_folder(db, generated, series.season_year, Some(series.id))
                .await
                .map_err(|e| e.to_string())?;
        // Persist it so future imports skip this path.
        let _ = series::update_folder(db, series.id, &generated).await;
        generated
    } else {
        series.folder_name.clone()
    };

    let folder_name = if series.library_subdir.is_empty() {
        folder_name
    } else {
        format!("{}/{}", series.library_subdir, folder_name)
    };
    if !media::usable_library_folder(&folder_name) {
        return Err("unsafe series library location".into());
    }
    let season_dir = Path::new(&cfg.media_root)
        .join(&folder_name)
        .join(naming::season_folder(
            &cfg.season_folder_format,
            &cfg.title_language,
            &naming::SeriesNames::from_series(series),
            1,
        ));
    Ok((folder_name, season_dir))
}
