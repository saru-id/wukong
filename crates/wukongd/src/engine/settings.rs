//! The settings half of the governor: reconcile the governed defaults
//! against acknowledged state, offer changes for recording, and serve
//! the desired-vs-live view that `wukong settings` renders. Same
//! single-writer rules as everything else on `Engine`.

use super::{Engine, refreshed, soft};
use std::collections::BTreeSet;
use wukong_core::db::InboxOutcome;
use wukong_core::events::{EventKind, InboxKind, Resolution};
use wukong_core::ipc::{CaptureChange, Response, SettingEntry};
use wukong_core::settings::{self, Value};

/// Pseudo-domain marking that the first settings reconcile has run;
/// real domains contain dots or are `NSGlobalDomain`, so no collision.
const BASELINE: &str = "__meta__";

/// A capture snapshot older than this answers with an error — diffing
/// against a stale world would attribute hours of churn to one click.
#[allow(clippy::duration_suboptimal_units)] // Duration::from_mins is unstable
const CAPTURE_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// A diff bigger than this is not "one setting changed"; cap the
/// response and say so.
const CAPTURE_MAX_CHANGES: usize = 200;

/// What a quarantined-change offer carries so resolve can act exactly
/// on what was offered.
#[derive(serde::Serialize, serde::Deserialize)]
struct SettingMeta {
    domain: String,
    key: String,
    value: Value,
}

impl Engine {
    /// Compare reality against acknowledged state and offer only the
    /// TRANSITIONS: a governed setting whose value changed becomes a
    /// "record it?" offer — unless the manifest already wants exactly
    /// that value (a sync landing, or the user setting it by hand), or
    /// the key is ignored. The first run baselines silently: a
    /// long-tuned Mac must not open ninety offers.
    pub fn reconcile_settings(&mut self) -> usize {
        self.settings_dirty = None;
        let Some(prefs_dir) = self.prefs_dir.clone() else {
            return 0;
        };
        if self.settings_poisoned {
            return 0;
        }
        let mut wanted = self.settings_manifest.governed_keys();
        wanted.extend(self.shared_settings.governed_keys());
        let current = settings::read_current(&prefs_dir, &wanted);
        let state = match self.db.settings_state() {
            Ok(state) => state,
            Err(e) => {
                soft(Err::<(), _>(e));
                return 0;
            }
        };
        let baseline = !state.contains_key(&(BASELINE.to_string(), BASELINE.to_string()));
        if baseline {
            soft(self.db.settings_state_set(BASELINE, BASELINE, "done"));
        }
        let mut new_items = 0;
        for (id, live) in &current {
            let (domain, key) = id;
            let live_json = serde_json::to_string(live).unwrap_or_default();
            let acknowledged = state.get(id).and_then(|s| serde_json::from_str(s).ok());
            if acknowledged
                .as_ref()
                .is_some_and(|a: &Value| a.matches(live))
            {
                continue; // no transition
            }
            soft(self.db.settings_state_set(domain, key, &live_json));
            if baseline
                || self.setting_ignored(domain, key)
                || self
                    .desired_setting(domain, key)
                    .is_some_and(|want| want.matches(live))
            {
                // Returning to the desired value (a sync landing, a
                // manual revert) makes any open offer stale — resolve
                // it rather than leaving a ghost to approve.
                soft(self.db.inbox_resolve_open(
                    InboxKind::Setting,
                    &format!("{domain} {key}"),
                    Resolution::Skip,
                ));
                continue;
            }
            new_items += self.offer_setting(domain, key, live, acknowledged.as_ref());
        }
        // Keys that vanished (reverted to OS default): acknowledge
        // silently — absence is not a value worth an offer.
        for (id, _) in state {
            if id.0 != BASELINE && !current.contains_key(&id) {
                soft(self.db.settings_state_remove(&id.0, &id.1));
            }
        }
        new_items
    }

    fn offer_setting(
        &mut self,
        domain: &str,
        key: &str,
        live: &Value,
        was: Option<&Value>,
    ) -> usize {
        let label = settings::known(domain, key).map_or("(outside the corpus)", |k| k.label);
        let from = was.map_or_else(|| "(unset)".to_string(), ToString::to_string);
        let body = format!(
            "{label}\n{domain} {key}: {from} → {live}\n\n\
             approve — record {live} as this machine's desired value\n\
             never   — don't ask about this setting again\n\
             skip    — not now"
        );
        let meta = serde_json::to_string(&SettingMeta {
            domain: domain.to_string(),
            key: key.to_string(),
            value: live.clone(),
        })
        .unwrap_or_default();
        let outcome = self
            .db
            .inbox_add(
                InboxKind::Setting,
                &format!("{domain} {key}"),
                "setting changed — record it?",
                &body,
                &meta,
            )
            .unwrap_or_else(refreshed);
        usize::from(outcome == InboxOutcome::New)
    }

    /// Capture phase 1: snapshot every top-level scalar key across
    /// every preference domain, in memory only.
    pub(super) fn capture_start(&mut self) -> Response {
        let Some(prefs_dir) = self.prefs_dir.clone() else {
            return Response::Error {
                message: "settings governance is disabled in config".to_string(),
            };
        };
        let snapshot = settings::read_all(&prefs_dir);
        let domains: std::collections::BTreeSet<&str> =
            snapshot.keys().map(|(d, _)| d.as_str()).collect();
        let message = format!(
            "snapshotted {} keys across {} domains",
            snapshot.len(),
            domains.len()
        );
        self.capture = Some((std::time::Instant::now(), snapshot));
        Response::Ok { message }
    }

    /// Capture phase 2: diff reality against the snapshot, classify
    /// noise, consume the snapshot.
    pub(super) fn capture_diff(&mut self) -> Response {
        let Some(prefs_dir) = self.prefs_dir.clone() else {
            return Response::Error {
                message: "settings governance is disabled in config".to_string(),
            };
        };
        let Some((taken, snapshot)) = self.capture.take() else {
            return Response::Error {
                message: "no capture in progress — start with `wukong settings capture`"
                    .to_string(),
            };
        };
        if taken.elapsed() > CAPTURE_TTL {
            return Response::Error {
                message: "capture snapshot expired (10 minutes) — start again".to_string(),
            };
        }
        let now = settings::read_all(&prefs_dir);
        let mut changes = Vec::new();
        for (id, after) in &now {
            let before = snapshot.get(id);
            if before.is_some_and(|b| b.matches(after)) {
                continue;
            }
            changes.push(change(id, before.cloned(), Some(after.clone())));
        }
        for (id, before) in &snapshot {
            if !now.contains_key(id) {
                changes.push(change(id, Some(before.clone()), None));
            }
        }
        // Signal first, then alphabetical; bounded.
        changes.sort_by(|a, b| (a.noise, &a.domain, &a.key).cmp(&(b.noise, &b.domain, &b.key)));
        changes.truncate(CAPTURE_MAX_CHANGES);
        Response::CaptureDiff { changes }
    }

    /// Setting inbox items: approve records the offered value into the
    /// manifest; ignore is the PERMANENT per-key opt-out.
    pub(super) fn resolve_setting(
        &mut self,
        id: i64,
        meta: &str,
        resolution: Resolution,
    ) -> Response {
        if matches!(resolution, Resolution::Redact | Resolution::Seal) {
            return Response::Error {
                message: "only approve or ignore applies to settings".to_string(),
            };
        }
        let Ok(meta) = serde_json::from_str::<SettingMeta>(meta) else {
            return Response::Error {
                message: "malformed setting item".to_string(),
            };
        };
        soft(self.db.inbox_resolve(id, resolution));
        let subject = format!("{} {}", meta.domain, meta.key);
        soft(
            self.db
                .record(EventKind::Resolved, &subject, resolution.as_str()),
        );
        match resolution {
            Resolution::Approve => {
                self.settings_manifest
                    .set(&meta.domain, &meta.key, meta.value.clone());
                soft(self.db.record(
                    EventKind::SettingRecorded,
                    &subject,
                    &meta.value.to_string(),
                ));
                self.commit_settings_manifest(&format!(
                    "{} {} = {}",
                    meta.domain, meta.key, meta.value
                ));
            }
            Resolution::Never => {
                self.settings_manifest.add_ignore(&meta.domain, &meta.key);
                soft(
                    self.db
                        .record(EventKind::SettingIgnored, &subject, "from inbox"),
                );
                self.commit_settings_manifest(&format!("ignore {} {}", meta.domain, meta.key));
            }
            // Skip: the item is closed, the acknowledged state already
            // matches reality — nothing more to do, nothing promised.
            Resolution::Skip => {}
            Resolution::Redact | Resolution::Seal => unreachable!("rejected above"),
        }
        Response::Ok {
            message: format!("resolved {subject} ({})", resolution.as_str()),
        }
    }

    /// The desired-vs-live view: every corpus and manifest key.
    pub(super) fn settings_list(&self) -> Response {
        let Some(prefs_dir) = self.prefs_dir.clone() else {
            return Response::Error {
                message: "settings governance is disabled in config".to_string(),
            };
        };
        let mut wanted = self.settings_manifest.governed_keys();
        wanted.extend(self.shared_settings.governed_keys());
        let current = settings::read_current(&prefs_dir, &wanted);
        let entries = wanted
            .into_iter()
            .map(|(domain, key)| {
                let known = settings::known(&domain, &key);
                let desired = self.desired_setting(&domain, &key).cloned();
                let live = current.get(&(domain.clone(), key.clone())).cloned();
                let in_sync = match (&desired, &live) {
                    (Some(want), Some(is)) => want.matches(is),
                    _ => false,
                };
                SettingEntry {
                    label: known.map(|k| k.label.to_string()),
                    restart: known
                        .and_then(|k| k.restart)
                        .map(str::to_string)
                        .or_else(|| {
                            self.settings_manifest
                                .restart_of(&domain, &key)
                                .or_else(|| self.shared_settings.restart_of(&domain, &key))
                                .map(str::to_string)
                        }),
                    domain,
                    key,
                    desired,
                    live,
                    in_sync,
                }
            })
            .collect();
        Response::Settings {
            entries,
            file_domains_dir: self
                .config
                .settings
                .preferences_dir
                .as_ref()
                .map(|d| d.to_string_lossy().into_owned()),
        }
    }

    /// Record a setting's current live value as desired — the explicit
    /// path for keys outside the corpus (or ahead of the watcher).
    pub(super) fn settings_record(
        &mut self,
        domain: &str,
        key: &str,
        restart: Option<&str>,
    ) -> Response {
        let Some(prefs_dir) = self.prefs_dir.clone() else {
            return Response::Error {
                message: "settings governance is disabled in config".to_string(),
            };
        };
        let wanted: BTreeSet<_> = [(domain.to_string(), key.to_string())].into();
        let Some(value) = settings::read_current(&prefs_dir, &wanted)
            .remove(&(domain.to_string(), key.to_string()))
        else {
            return Response::Error {
                message: format!("{domain} {key} is unset or not a scalar wukong can govern"),
            };
        };
        let json = serde_json::to_string(&value).unwrap_or_default();
        soft(self.db.settings_state_set(domain, key, &json));
        self.settings_manifest.set(domain, key, value.clone());
        // Restart knowledge, best-effort: explicit flag beats the
        // corpus beats domain inference. Corpus keys carry their own;
        // only strays need a stored hint.
        let hint = restart.map(str::to_string).or_else(|| {
            if settings::known(domain, key).is_some() {
                None
            } else {
                settings::restart_for_domain(domain).map(str::to_string)
            }
        });
        if let Some(process) = hint {
            self.settings_manifest.set_restart(domain, key, &process);
        }
        let subject = format!("{domain} {key}");
        soft(
            self.db
                .record(EventKind::SettingRecorded, &subject, &value.to_string()),
        );
        soft(
            self.db
                .inbox_resolve_open(InboxKind::Setting, &subject, Resolution::Approve),
        );
        self.commit_settings_manifest(&format!("{domain} {key} = {value}"));
        Response::Ok {
            message: format!("recorded {subject} = {value}"),
        }
    }

    #[allow(clippy::used_underscore_items)]
    pub(super) fn settings_ignore(&mut self, domain: &str, key: &str, unignore: bool) -> Response {
        let subject = format!("{domain} {key}");
        if unignore {
            if !self.settings_manifest.remove_ignore(domain, key) {
                return Response::Error {
                    message: format!("{subject} was not ignored"),
                };
            }
            self.commit_settings_manifest(&format!("unignore {subject}"));
            Response::Ok {
                message: format!("{subject} can be offered again"),
            }
        } else {
            self.settings_manifest.add_ignore(domain, key);
            soft(self.db.record(EventKind::SettingIgnored, &subject, ""));
            soft(
                self.db
                    .inbox_resolve_open(InboxKind::Setting, &subject, Resolution::Skip),
            );
            self.commit_settings_manifest(&format!("ignore {subject}"));
            Response::Ok {
                message: format!("{subject} will never be offered again"),
            }
        }
    }

    /// This machine's manifest wins per key; the shared lane fills
    /// the rest.
    fn desired_setting(&self, domain: &str, key: &str) -> Option<&Value> {
        self.settings_manifest
            .desired(domain, key)
            .or_else(|| self.shared_settings.desired(domain, key))
    }

    fn setting_ignored(&self, domain: &str, key: &str) -> bool {
        self.settings_manifest.ignored(domain, key) || self.shared_settings.ignored(domain, key)
    }

    /// Move a recorded setting between the machine and shared lanes.
    pub(super) fn setting_share(&mut self, domain: &str, key: &str, undo: bool) -> Response {
        let subject = format!("{domain} {key}");
        if undo {
            let Some(value) = self.shared_settings.desired(domain, key).cloned() else {
                return Response::Error {
                    message: format!("{subject} is not in the shared lane"),
                };
            };
            let hint = self
                .shared_settings
                .restart_of(domain, key)
                .map(str::to_string);
            self.shared_settings.remove(domain, key);
            self.settings_manifest.set(domain, key, value);
            if let Some(process) = hint {
                self.settings_manifest.set_restart(domain, key, &process);
            }
            self.commit_settings_manifest(&format!("{subject} joins the machine lane"));
            self.commit_shared_settings(&format!("{subject} moved to the machine lane"));
        } else {
            let Some(value) = self.settings_manifest.desired(domain, key).cloned() else {
                return Response::Error {
                    message: format!(
                        "{subject} has no recorded value on this machine — record it first"
                    ),
                };
            };
            let hint = self
                .settings_manifest
                .restart_of(domain, key)
                .map(str::to_string);
            self.settings_manifest.remove(domain, key);
            self.shared_settings.set(domain, key, value);
            if let Some(process) = hint {
                self.shared_settings.set_restart(domain, key, &process);
            }
            self.commit_shared_settings(&format!("{subject} joins the shared lane"));
            self.commit_settings_manifest(&format!("{subject} moved to the shared lane"));
        }
        soft(self.db.record(
            EventKind::Shared,
            &subject,
            if undo { "machine lane" } else { "shared lane" },
        ));
        Response::Ok {
            message: format!(
                "{subject} now syncs to the {} lane",
                if undo { "machine" } else { "shared" }
            ),
        }
    }

    /// The shared twin of `commit_settings_manifest`.
    fn commit_shared_settings(&mut self, summary: &str) {
        let shared = self.store.shared();
        soft(self.shared_settings.save(shared.dir()));
        let store = shared;
        match store.commit(
            std::path::Path::new(settings::MANIFEST_REL),
            &format!("shared settings: {summary}"),
        ) {
            Ok(Some(sha)) => {
                soft(
                    self.db
                        .record(EventKind::Committed, "shared settings", summary),
                );
                self.last_commit = Some(sha);
                self.commits += 1;
                if self.remote_configured() {
                    self.unpushed += 1;
                    self.dirty = true;
                }
            }
            Ok(None) => {}
            Err(e) => soft(Err::<(), _>(e)),
        }
    }

    /// Persist the settings manifest into the store, gated and
    /// committed like everything else that reaches git.
    fn commit_settings_manifest(&mut self, summary: &str) {
        if self.settings_poisoned {
            soft(Err::<(), _>(
                "settings manifest on disk is unparseable — fix or delete it; not saving",
            ));
            return;
        }
        soft(self.settings_manifest.save(self.store.dir()));
        match self.store.commit(
            std::path::Path::new(settings::MANIFEST_REL),
            &format!("settings: {summary}"),
        ) {
            Ok(Some(sha)) => {
                soft(self.db.record(EventKind::Committed, "settings", summary));
                self.last_commit = Some(sha);
                self.commits += 1;
                if self.remote_configured() {
                    self.unpushed += 1;
                    self.dirty = true;
                }
            }
            Ok(None) => {}
            Err(e) => soft(Err::<(), _>(e)),
        }
    }
}

/// Build one classified capture change.
fn change(id: &(String, String), before: Option<Value>, after: Option<Value>) -> CaptureChange {
    let (domain, key) = id;
    CaptureChange {
        noise: settings::is_noise_key(domain, key),
        label: settings::known(domain, key).map(|k| k.label.to_string()),
        domain: domain.clone(),
        key: key.clone(),
        before,
        after,
    }
}
