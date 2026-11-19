//! Persist the import's resolved numbering in its existing episode NFO.
//! Original torrent filenames may use absolute numbers or a foreign season;
//! only NFOs explicitly marked by an in-place import override the file parser.
use crate::services::media::{EpisodeSpan, MAX_FILE_SPAN};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use tokio::io::AsyncWriteExt;

const MARKER: &str = "<!-- ryokan-library-episodes:";

pub async fn annotate_nfo(nfo: &Path, season: i32, first: i32, last: i32) -> std::io::Result<()> {
    let mut file = tokio::fs::OpenOptions::new().append(true).open(nfo).await?;
    file.write_all(format!("{MARKER}{season}:{first}:{last} -->\n").as_bytes())
        .await?;
    file.flush().await
}

/// Called on the scanner's blocking thread. Read at most the last 256 bytes
/// rather than loading arbitrary NFO content supplied alongside a release.
pub fn read_span(video: &Path) -> Option<EpisodeSpan> {
    let mut file = std::fs::File::open(video.with_extension("nfo")).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(256))).ok()?;
    let mut bytes = Vec::new();
    file.take(256).read_to_end(&mut bytes).ok()?;
    let tail = String::from_utf8_lossy(&bytes);
    let (_, marker) = tail.rsplit_once(MARKER)?;
    let (numbers, _) = marker.split_once(" -->")?;
    let mut values = numbers.split(':').map(str::parse::<i32>);
    let season = values.next()?.ok()?;
    let first = values.next()?.ok()?;
    let last = values.next()?.ok()?;
    if values.next().is_some()
        || season != 1
        || first < 1
        || last < first
        || last.saturating_sub(first) > MAX_FILE_SPAN
    {
        return None;
    }
    Some(EpisodeSpan {
        season: Some(season),
        first,
        last,
        special: false,
    })
}

/// Reuse the library scanner for existing files, including a torrent's
/// subfolder and its persisted relative episode numbers. Runs off-runtime.
pub fn existing_episodes(season: &Path) -> Vec<(std::path::PathBuf, EpisodeSpan)> {
    let Some(root) = season.parent().and_then(Path::to_str) else {
        return Vec::new();
    };
    let Some(folder) = season.file_name().and_then(|p| p.to_str()) else {
        return Vec::new();
    };
    let Ok(real_season) = season.canonicalize() else {
        return Vec::new();
    };
    crate::services::media::scan_series_folder_split(root, folder)
        .0
        .into_iter()
        .filter_map(|file| {
            let path = season.join(file.filename);
            if !path
                .canonicalize()
                .is_ok_and(|real| real.starts_with(&real_season))
            {
                return None;
            }
            Some((
                path,
                EpisodeSpan {
                    season: file.season_number,
                    first: file.episode_number,
                    last: file.episode_last,
                    special: false,
                },
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn direct_library_nfo_keeps_resolved_numbers_for_original_filenames() {
        let root = tempfile::tempdir().unwrap();
        let season = root.path().join("Show/Season 01");
        std::fs::create_dir_all(&season).unwrap();
        let video = season.join("Show - S03E25-E26.mkv");
        std::fs::write(&video, b"video").unwrap();
        std::fs::write(
            video.with_extension("nfo"),
            "<episodedetails><episode>1</episode></episodedetails>\n",
        )
        .unwrap();
        // An ordinary release-supplied NFO never changes the file parser.
        assert!(read_span(&video).is_none());
        annotate_nfo(&video.with_extension("nfo"), 1, 1, 2)
            .await
            .unwrap();
        let (files, _) =
            crate::services::media::scan_series_folder_split(root.path().to_str().unwrap(), "Show");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].episode_number, 1);
        assert_eq!(files[0].episode_last, 2);
        assert_eq!(files[0].season_number, Some(1));
    }

    #[test]
    fn direct_library_metadata_rejects_invalid_spans() {
        let root = tempfile::tempdir().unwrap();
        let video = root.path().join("Show.mkv");
        for numbers in ["1:0:2", "1:5:1", "-1:1:1", "1:1:2147483647", "1:1:2:3"] {
            std::fs::write(
                video.with_extension("nfo"),
                format!("{MARKER}{numbers} -->"),
            )
            .unwrap();
            assert!(read_span(&video).is_none(), "{numbers}");
        }
    }
}
