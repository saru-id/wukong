//! The governor governing itself. An unattended daemon's failures
//! must escalate through the same inbox everything else uses — a push
//! that has been failing for a day, a quarantined secret nobody has
//! answered in a week, a store git can no longer walk. Silence and
//! calm are not the same thing.

use super::{Engine, refreshed, soft};
use wukong_core::db::InboxOutcome;
use wukong_core::events::{EventKind, InboxKind};

/// How often the checks run at all.
#[allow(clippy::duration_suboptimal_units)] // Duration::from_hours is unstable
const HEALTH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3600);

/// A push that hasn't SUCCEEDED for this long, with failures on
/// record, is a broken remote — not a quiet day.
const PUSH_STALE_SECS: i64 = 24 * 3600;

/// A quarantined change waiting longer than this gets a reminder.
const QUARANTINE_STALE_SECS: i64 = 7 * 24 * 3600;

/// One nag per subject per this window — resolved or not.
const REALERT_SECS: i64 = 24 * 3600;

/// Seconds since an RFC3339 timestamp; `None` when unparseable.
fn age_secs(ts: &str) -> Option<i64> {
    let then: jiff::Timestamp = ts.parse().ok()?;
    Some(jiff::Timestamp::now().duration_since(then).as_secs())
}

impl Engine {
    /// Run the self-checks, at most once per hour. Returns new inbox
    /// items so the caller's notification path treats them like any
    /// other news.
    pub fn health_tick(&mut self) -> usize {
        if self.last_health.elapsed() < HEALTH_INTERVAL {
            return 0;
        }
        self.last_health = std::time::Instant::now();
        self.health_checks()
    }

    /// The checks themselves, gate-free — what tests call.
    #[cfg(test)]
    pub(super) fn health_tick_forced(&mut self) -> usize {
        self.health_checks()
    }

    fn health_checks(&mut self) -> usize {
        self.check_push_health() + self.check_quarantine_age() + self.check_crash_loop()
    }

    /// A Health item files at most once per REALERT window per
    /// subject, whether or not the previous one was answered.
    fn health_alert(&mut self, subject: &str, detail: &str, body: &str) -> usize {
        let recent = self
            .db
            .last_event_for(EventKind::Health, subject)
            .ok()
            .flatten()
            .and_then(|ts| age_secs(&ts))
            .is_some_and(|age| age < REALERT_SECS);
        if recent {
            return 0;
        }
        soft(self.db.record(EventKind::Health, subject, detail));
        let outcome = self
            .db
            .inbox_add(InboxKind::Health, subject, detail, body, "")
            .unwrap_or_else(refreshed);
        usize::from(outcome == InboxOutcome::New)
    }

    /// Health items: the governor's own notices. approve runs the
    /// obvious fix where one exists (queue a push); skip dismisses
    /// until the condition re-fires after its re-alert window.
    pub(super) fn resolve_health(
        &mut self,
        id: i64,
        subject: &str,
        resolution: wukong_core::events::Resolution,
    ) -> wukong_core::ipc::Response {
        use wukong_core::events::Resolution;
        use wukong_core::ipc::Response;
        if !matches!(resolution, Resolution::Approve | Resolution::Skip) {
            return Response::Error {
                message: "health notices take approve or skip".to_string(),
            };
        }
        soft(self.db.inbox_resolve(id, resolution));
        soft(
            self.db
                .record(EventKind::Resolved, subject, resolution.as_str()),
        );
        if resolution == Resolution::Approve && subject == "push" && self.remote_configured() {
            self.dirty = true;
            return Response::Ok {
                message: "push queued — `wukong status` shows the result".to_string(),
            };
        }
        Response::Ok {
            message: format!("noted ({})", resolution.as_str()),
        }
    }

    pub(super) fn check_push_health(&mut self) -> usize {
        if !self.remote_configured() || self.unpushed == 0 {
            return 0;
        }
        let last_success = self.db.last_event(EventKind::Pushed).ok().flatten();
        let stale = match last_success {
            Some(ts) => age_secs(&ts).is_some_and(|age| age > PUSH_STALE_SECS),
            // Never pushed at all: stale once the oldest failure is.
            None => true,
        };
        let failed = self
            .db
            .last_event(EventKind::PushFailed)
            .ok()
            .flatten()
            .is_some();
        if !(stale && failed) {
            return 0;
        }
        let last_error = self
            .db
            .events(200)
            .ok()
            .and_then(|events| {
                events
                    .into_iter()
                    .find(|e| e.kind == EventKind::PushFailed.as_str())
                    .map(|e| e.detail)
            })
            .unwrap_or_default();
        self.health_alert(
            "push",
            "pushes have been failing for a day",
            &format!(
                "{} commit(s) are stuck on this machine; the remote has not \
                 accepted a push in over 24h.\n\nlast error:\n  {last_error}\n\n\
                 approve — try a push right now\n\
                 skip    — dismiss for a day (check `wukong doctor`)",
                self.unpushed
            ),
        )
    }

    fn check_quarantine_age(&mut self) -> usize {
        let Ok(items) = self.db.inbox_open() else {
            return 0;
        };
        let stale = items
            .iter()
            .filter(|i| i.kind() == Some(InboxKind::Quarantine))
            .filter(|i| age_secs(&i.ts).is_some_and(|age| age > QUARANTINE_STALE_SECS))
            .count();
        if stale == 0 {
            return 0;
        }
        self.health_alert(
            "quarantine",
            "quarantined changes are waiting",
            &format!(
                "{stale} quarantined change(s) have waited over a week — their \
                 files are tracked but their latest edits are NOT in the store, \
                 so a lost disk loses them.\n\n\
                 approve — nothing to run; open `wukong` and answer them\n\
                 skip    — dismiss for a day"
            ),
        )
    }

    /// The one failure the watchdog can't see from inside: itself
    /// dying repeatedly while launchd dutifully restarts it. Clustered
    /// start events are that story told from the outside.
    fn check_crash_loop(&mut self) -> usize {
        let Ok(events) = self.db.events(100) else {
            return 0;
        };
        let recent_starts = events
            .iter()
            .filter(|e| e.kind == EventKind::DaemonStarted.as_str())
            .filter(|e| age_secs(&e.ts).is_some_and(|age| age < 3600))
            .count();
        if recent_starts < 6 {
            return 0;
        }
        self.health_alert(
            "daemon",
            "the daemon is crash-looping",
            &format!(
                "{recent_starts} daemon starts in the last hour — launchd keeps \
                 restarting a process that keeps dying. The log usually says why:\n  \
                 ~/.local/state/wukong/wukongd.log\n\n\
                 approve — nothing to run; read the log\n\
                 skip    — dismiss for a day"
            ),
        )
    }

    /// A machine whose seal identity cannot decrypt what this store
    /// encrypts looks healthy right up until the day the restore
    /// matters. Probed on the rescan cadence, not at disaster time.
    pub(super) fn check_seal_health(&mut self) -> usize {
        let any_sealed = self
            .db
            .tracked()
            .is_ok_and(|rows| rows.iter().any(|(_, sealed, _)| *sealed));
        if !any_sealed {
            return 0;
        }
        let recipient =
            std::fs::read_to_string(self.store.dir().join(wukong_core::seal::RECIPIENT_REL));
        let id_store = wukong_core::seal::IdentityStore::from_config(
            self.config.seal.identity_file.as_deref(),
        );
        let unlocks = match (id_store.load(), recipient) {
            (Ok(Some(identity)), Ok(recipient)) => {
                wukong_core::seal::encrypt(recipient.trim(), b"probe")
                    .is_ok_and(|probe| wukong_core::seal::decrypt(&identity, &probe).is_ok())
            }
            _ => false,
        };
        if unlocks {
            // The key works — but does it exist ANYWHERE else? Sealed
            // files with an unbacked-up identity are one dead disk
            // from unrecoverable.
            let escrow = self
                .store
                .shared()
                .dir()
                .join(wukong_core::seal::ESCROW_REL)
                .is_file();
            if !escrow {
                return self.health_alert(
                    "seal-backup",
                    "the seal identity has no backup",
                    "This machine holds the only copy of the key that unlocks your \
                     sealed files. `wukong seal-key backup` escrows it in the store, \
                     passphrase-encrypted — losing every machine then costs one \
                     passphrase, not the files.\n\n\
                     approve — nothing to run; back it up when convenient\n\
                     skip    — dismiss for a day",
                );
            }
            return 0;
        }
        self.health_alert(
            "seal",
            "sealed files cannot be decrypted on this machine",
            "This machine tracks sealed files, but its seal identity is \
             missing or does not match the store's recipient — a restore \
             would fail exactly when it matters.\n\n\
             approve — nothing to run; `wukong seal-key import` the right key\n\
             skip    — dismiss for a day",
        )
    }

    /// The rescan-cadence probe: a store git cannot walk anymore must
    /// not fail silently at push time three weeks later.
    pub(super) fn check_store_integrity(&mut self) -> usize {
        if let Err(e) = self.store.fsck() {
            return self.health_alert(
                "store",
                "the store repository is damaged",
                &format!(
                    "git fsck failed on the mirror store:\n  {e}\n\n\
                     The remote still has everything pushed so far. Simplest \
                     repair: move ~/.local/share/wukong/store aside and run \
                     `wukong init` to re-clone.\n\n\
                     approve — nothing to run automatically; repair by hand\n\
                     skip    — dismiss for a day"
                ),
            );
        }
        0
    }
}
