//! Add-time options shared by ordinary, picker and selective grabs.
use super::*;

impl QbitClient {
    pub(super) async fn add_at(
        &self,
        url: &str,
        info_hash: &str,
        destination: Option<&str>,
    ) -> Result<AddOutcome, String> {
        super::super::check_release_url(url)?;
        let mut destination = destination;
        if info_hash.is_empty() {
            destination = None;
        }
        if !info_hash.is_empty() {
            super::super::check_info_hash(info_hash)?;
            // qBittorrent 4.x answers `Ok.` to a duplicate add as well, so
            // the add's own answer can't say whether this call created the
            // torrent. The file picker took that `Ok.` to mean "ours" and
            // its cancel deleted the torrent with its data, which could be
            // another grab's download. Ask first.
            match self.lookup_torrent(info_hash).await {
                Ok(true) => return Ok(AddOutcome::AlreadyPresent),
                Err(e) if destination.is_some() => return Err(e),
                _ => {}
            }
        }
        let resolved_destination = match destination {
            Some(path) => Some(self.resolve_library_destination(path).await?),
            None => None,
        };
        let destination = resolved_destination.as_deref();
        let mut form = vec![("urls", url), ("category", self.category.as_str())];
        if let Some(path) = destination {
            form.push(("savepath", path));
            form.push(("autoTMM", "false"));
        }
        tracing::debug!(
            info_hash,
            category = %self.category,
            savepath = ?destination,
            auto_tmm = ?destination.map(|_| false),
            "qbit add options (savepath=None uses client defaults)"
        );
        let resp = self.do_post_form("/api/v2/torrents/add", &form).await?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            return Err(format!("qbit add failed (HTTP {status}): {body}"));
        }
        // qBit returns 200 OK with body "Fails." in two different
        // scenarios that are indistinguishable from the response
        // alone: a genuinely-malformed magnet (bad hash length, bad
        // URN scheme) and — the case live-verified against v5.1.4
        // during the Phase 1 download-client smoke work — a *duplicate*
        // magnet whose info-hash is already in the session. Older
        // builds are silent about duplicates (the "always 200 Ok."
        // behavior the module-header comment describes); v5.x changed
        // that, and the auto-search path lit up with
        // `qBit returned 'Fails.'` errors on every re-grab of a
        // torrent already in the client (e.g. RSS re-emitting an item
        // whose grab row was lost on a crash, or a manual grab +
        // upgrade-sweep hitting the same release).
        //
        // We can disambiguate after the fact: look the info-hash up
        // via `/torrents/info?hashes=`. Present → duplicate, report
        // `AlreadyPresent` and let the caller record the grab
        // normally. Missing → the magnet really was rejected; surface
        // the error so auto-search backs off.
        if body.trim() == "Fails." {
            if !info_hash.is_empty() && self.has_torrent(info_hash).await {
                return Ok(AddOutcome::AlreadyPresent);
            }
            return Err(format!(
                "qbit add rejected url={}: qBit returned 'Fails.'",
                crate::services::sanitize::redact_url(url)
            ));
        }
        self.invalidate_torrents_cache().await;
        // On the 200 "Ok." path qBit returns the same body whether the
        // hash was new or (on older builds) a silent duplicate. We
        // report `Added` either way; the `AlreadyPresent` path only
        // fires for the v5.x "Fails." duplicate disambiguation above.
        Ok(AddOutcome::Added)
    }
    pub(super) async fn paused_at(
        &self,
        url: &str,
        info_hash: &str,
        destination: Option<&str>,
    ) -> Result<AddOutcome, String> {
        super::super::check_release_url(url)?;
        if !info_hash.is_empty() {
            super::super::check_info_hash(info_hash)?;
        }
        if info_hash.is_empty() {
            return Err("qBit paused add requires a pre-computed info hash".into());
        }
        let hash_lc = info_hash.to_ascii_lowercase();

        let outcome = self.add_at(url, &hash_lc, destination).await?;

        // Don't touch a pre-existing torrent's priorities. The user
        // may have partial-downloaded this release from an earlier
        // grab with careful file-selection; mark-all-skip would wipe
        // that and force them to re-pick. Handler is responsible for
        // surfacing the existing state to the modal instead (same-
        // hash dedup flow, plan decision #6).
        if outcome == AddOutcome::AlreadyPresent {
            return Ok(outcome);
        }

        // Explicit resume so a fresh add that landed in stopped state
        // starts flowing metadata. Matches the pattern in
        // `add_torrent_with_file_filter`.
        let _ = self.resume(&hash_lc).await;

        match self
            .wait_for_metadata(&hash_lc, Duration::from_secs(10))
            .await
        {
            Ok(files) => {
                // Mark every file skipped so the torrent idles until
                // the caller flips selections back via set_file_wanted.
                let all_ids: Vec<usize> = (0..files.len()).collect();
                if !all_ids.is_empty() {
                    self.set_file_priority(&hash_lc, &all_ids, 0).await?;
                }
            }
            Err(_) => {
                // Metadata timeout — leave the torrent running at
                // default priorities. Handler will see an empty /
                // not-yet-populated file list on the GET preview
                // endpoint and surface the timeout dialog to the
                // user. No data loss, just a UX prompt.
            }
        }

        Ok(outcome)
    }
    pub(super) async fn filtered_at(
        &self,
        url: &str,
        info_hash: &str,
        destination: Option<&str>,
        pick: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String> {
        super::super::check_release_url(url)?;
        if info_hash.is_empty() {
            return Err("selective download requires a known info hash".into());
        }
        let hash_lc = info_hash.to_ascii_lowercase();

        let outcome = self.add_at(url, &hash_lc, destination).await?;
        if destination.is_some() && outcome == AddOutcome::AlreadyPresent {
            return Ok(SelectiveOutcome::FullDownload);
        }

        // If qBit already had this hash sitting in stopped state,
        // the add above is a dedup no-op and the torrent is still
        // stopped. Explicitly start it so metadata flows.
        self.resume(&hash_lc).await?;

        let files = match self
            .wait_for_metadata(&hash_lc, Duration::from_secs(10))
            .await
        {
            Ok(files) => files,
            Err(_) => return Ok(SelectiveOutcome::FullDownload),
        };

        let names: Vec<String> = files.iter().map(|f| f.name.clone()).collect();
        let keep = pick(&names);

        let new_keep_ids = match keep {
            Some(ids) if !ids.is_empty() && ids.len() < files.len() => ids,
            _ => return Ok(SelectiveOutcome::FullDownload),
        };

        let already_narrowed = files.iter().any(|f| f.priority == 0);

        if already_narrowed {
            let to_bump: Vec<usize> = new_keep_ids
                .iter()
                .copied()
                .filter(|&i| files.get(i).map(|f| f.priority == 0).unwrap_or(false))
                .collect();
            if !to_bump.is_empty() {
                self.set_file_priority(&hash_lc, &to_bump, 1).await?;
            }
        } else {
            let skip_ids: Vec<usize> = (0..files.len())
                .filter(|i| !new_keep_ids.contains(i))
                .collect();
            self.set_file_priority(&hash_lc, &skip_ids, 0).await?;
            self.set_file_priority(&hash_lc, &new_keep_ids, 1).await?;
        }
        Ok(SelectiveOutcome::Filtered(new_keep_ids))
    }
}
