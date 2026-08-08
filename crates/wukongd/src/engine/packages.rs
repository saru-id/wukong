//! The package half of the governor: the reconcile state machine and
//! everything that edits the manifest. Split from the file flow so each
//! concern reads on its own; the state still lives on `Engine`, and the
//! single-writer rule is unchanged.

use super::{Engine, refreshed, soft};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use wukong_core::db::InboxOutcome;
use wukong_core::events::{EventKind, InboxKind, Resolution};
use wukong_core::gate::{self, GateVerdict};
use wukong_core::ipc::{PkgEntry, Response};
use wukong_core::pkg::{self, Provider};

/// Pseudo-provider row marking that the first package reconcile has
/// run; real providers are "formula"/"cask"/"app", so no collision.
const BASELINE_MARKER: &str = "__meta__";

/// How long a receiptless Cellar dir keeps re-arming the reconcile
/// before we stop waiting for its pour to finish.
#[allow(clippy::duration_suboptimal_units)] // Duration::from_mins is unstable
const UNSETTLED_WINDOW: std::time::Duration = std::time::Duration::from_secs(600);

impl Engine {
    /// Compare reality against the last acknowledged state and offer
    /// only the TRANSITIONS: a newly appeared package that isn't in
    /// the manifest or on the ignore list becomes an adoption offer; a
    /// vanished manifest member becomes a removal offer. The first run
    /// ever baselines silently — a machine full of pre-wukong installs
    /// must not open fifty inbox items (bulk adoption is an explicit
    /// CLI verb). Returns new inbox items.
    pub fn reconcile(&mut self) -> usize {
        self.pkg_dirty = None;
        if !self.config.packages.enabled {
            return 0;
        }
        // Homebrew can appear after the daemon started; pick it up.
        if self.pkg_roots.cellar.is_none() || self.pkg_roots.caskroom.is_none() {
            let fresh = self.config.pkg_roots();
            for root in fresh.watch_roots() {
                let canon = wukong_core::paths::canonicalize_lenient(&root);
                if !self.pkg_watch.contains(&canon) {
                    self.request_watch(&canon, false);
                    self.pkg_watch.push(canon);
                }
            }
            self.pkg_roots = fresh;
        }
        let current = self.pkg_roots.installed();
        self.pkg_installed.clone_from(&current);
        // A pour in progress (formula dir, no receipt yet) means the
        // interesting event — the receipt — will land too deep for the
        // watch. Re-arm so the next ticks re-check, bounded so a
        // permanently receiptless dir cannot spin the reconcile.
        if self
            .pkg_roots
            .cellar
            .as_deref()
            .is_some_and(pkg::unsettled_formulae)
        {
            let since = *self
                .pkg_unsettled_since
                .get_or_insert_with(std::time::Instant::now);
            if since.elapsed() < UNSETTLED_WINDOW {
                self.pkg_dirty = Some(std::time::Instant::now());
            }
        } else {
            self.pkg_unsettled_since = None;
        }
        // An explicit marker, not row-count inference: a machine whose
        // very first reconcile finds zero packages must still count as
        // baselined afterwards. A DB error must not masquerade as
        // "first run" — a silent re-baseline would swallow every real
        // transition this round.
        let baseline = match self.db.pkg_state(BASELINE_MARKER) {
            Ok(rows) => rows.is_empty(),
            Err(e) => {
                soft(Err::<(), _>(e));
                return 0;
            }
        };
        if baseline {
            soft(self.db.pkg_state_add(BASELINE_MARKER, "done"));
        }
        let mut new_items = 0;
        for (provider, installed) in current {
            let prev: BTreeSet<String> = self.db.pkg_state(provider.as_str()).unwrap_or_default();
            for name in installed.difference(&prev) {
                soft(self.db.pkg_state_add(provider.as_str(), name));
                // Reappearing resolves any stale "gone" offer — else
                // approving it later would drop an installed package.
                soft(self.db.inbox_resolve_open(
                    InboxKind::PackageGone,
                    &pkg::subject(provider, name),
                    Resolution::Ignore,
                ));
                if baseline
                    || self.manifest.contains(provider, name)
                    || self.manifest.ignored(provider, name)
                {
                    continue;
                }
                new_items += self.offer_package(provider, name);
            }
            for name in prev.difference(&installed) {
                soft(self.db.pkg_state_remove(provider.as_str(), name));
                // Disappearing resolves any stale adoption offer.
                soft(self.db.inbox_resolve_open(
                    InboxKind::Package,
                    &pkg::subject(provider, name),
                    Resolution::Ignore,
                ));
                if baseline || !self.manifest.contains(provider, name) {
                    continue;
                }
                new_items += self.offer_package_gone(provider, name);
            }
        }
        new_items
    }

    fn offer_package(&mut self, provider: Provider, name: &str) -> usize {
        let subject = pkg::subject(provider, name);
        let body = format!(
            "{name} ({}) was installed outside wukong.\n\n\
             approve — add it to the manifest; wukong remembers it\n\
             ignore  — never ask about {name} again",
            provider.as_str()
        );
        let outcome = self
            .db
            .inbox_add(
                InboxKind::Package,
                &subject,
                "installed outside wukong — adopt it?",
                &body,
                "",
            )
            .unwrap_or_else(refreshed);
        usize::from(outcome == InboxOutcome::New)
    }

    fn offer_package_gone(&mut self, provider: Provider, name: &str) -> usize {
        let subject = pkg::subject(provider, name);
        let body = format!(
            "{name} ({}) is in the manifest but no longer installed.\n\n\
             approve — drop it from the manifest\n\
             ignore  — keep it (pkg sync can reinstall it)",
            provider.as_str()
        );
        let outcome = self
            .db
            .inbox_add(
                InboxKind::PackageGone,
                &subject,
                "uninstalled outside wukong — drop it?",
                &body,
                "",
            )
            .unwrap_or_else(refreshed);
        soft(self.db.record(EventKind::PkgGone, &subject, ""));
        usize::from(outcome == InboxOutcome::New)
    }

    /// Persist the manifest into the store and commit it under the
    /// packages banner. Refused while the on-disk manifest is
    /// unparseable (saving would erase the real one), and gated like
    /// everything else that reaches a commit — a token-shaped package
    /// name or a poisoned clone must not ride into git.
    fn commit_manifest(&mut self, summary: &str) {
        if self.manifest_poisoned {
            soft(Err::<(), _>(
                "manifest on disk is unparseable — fix or delete it; not saving",
            ));
            return;
        }
        if let Ok(text) = toml::to_string_pretty(&self.manifest)
            && let GateVerdict::Quarantine(_) = gate::scan(Path::new(pkg::MANIFEST_REL), &text)
        {
            soft(self.db.record(
                EventKind::Held,
                "packages",
                "manifest failed the secret gate — not committed",
            ));
            return;
        }
        soft(self.manifest.save(self.store.dir()));
        match self.store.commit(
            Path::new(pkg::MANIFEST_REL),
            &format!("packages: {summary}"),
        ) {
            Ok(Some(sha)) => {
                soft(self.db.record(EventKind::Committed, "packages", summary));
                self.last_commit = Some(sha);
                self.commits += 1;
                self.unpushed += 1;
                self.dirty = true;
            }
            Ok(None) => {}
            Err(e) => soft(Err::<(), _>(e)),
        }
    }

    pub(super) fn pkg_record(
        &mut self,
        provider: Provider,
        name: &str,
        remove: bool,
        observe_only: bool,
    ) -> Response {
        let subject = pkg::subject(provider, name);
        if observe_only {
            // --no-track: acknowledge reality so the watcher does not
            // offer this install for adoption, but keep the manifest
            // untouched — the user opted out.
            if remove {
                soft(self.db.pkg_state_remove(provider.as_str(), name));
            } else {
                soft(self.db.pkg_state_add(provider.as_str(), name));
            }
            soft(
                self.db
                    .inbox_resolve_open(InboxKind::Package, &subject, Resolution::Ignore),
            );
            return Response::Ok {
                message: format!("{name} installed, not tracked (your call)"),
            };
        }
        if remove {
            self.manifest.remove(provider, name);
            soft(self.db.pkg_state_remove(provider.as_str(), name));
            soft(self.db.record(EventKind::PkgRemoved, &subject, ""));
            // An explicit removal supersedes any pending gone-offer.
            soft(
                self.db
                    .inbox_resolve_open(InboxKind::PackageGone, &subject, Resolution::Approve),
            );
            self.commit_manifest(&format!("-{name}"));
            Response::Ok {
                message: format!("{name} removed from the manifest"),
            }
        } else {
            self.manifest.add(provider, name);
            soft(self.db.pkg_state_add(provider.as_str(), name));
            soft(self.db.record(EventKind::PkgInstalled, &subject, ""));
            // An explicit install supersedes any pending adopt-offer.
            soft(
                self.db
                    .inbox_resolve_open(InboxKind::Package, &subject, Resolution::Approve),
            );
            self.commit_manifest(&format!("+{name}"));
            Response::Ok {
                message: format!("{name} recorded in the manifest"),
            }
        }
    }

    pub(super) fn pkg_list(&self) -> Response {
        // Served from the reconcile's cache: a TUI polling every two
        // seconds must not trigger a full Cellar walk each time.
        let installed: HashMap<Provider, BTreeSet<String>> =
            self.pkg_installed.iter().cloned().collect();
        Response::Packages {
            entries: self
                .manifest
                .entries()
                .into_iter()
                .map(|(provider, name)| PkgEntry {
                    installed: installed
                        .get(&provider)
                        .is_some_and(|set| set.contains(&name)),
                    provider,
                    name,
                })
                .collect(),
        }
    }

    pub(super) fn pkg_ignore(
        &mut self,
        provider: Provider,
        name: &str,
        unignore: bool,
    ) -> Response {
        let subject = pkg::subject(provider, name);
        if unignore {
            if !self.manifest.remove_ignore(provider, name) {
                return Response::Error {
                    message: format!("{name} was not ignored"),
                };
            }
            self.commit_manifest(&format!("unignore {name}"));
            Response::Ok {
                message: format!("{name} can be offered again"),
            }
        } else {
            self.manifest.add_ignore(provider, name);
            soft(self.db.record(EventKind::PkgIgnored, &subject, ""));
            soft(
                self.db
                    .inbox_resolve_open(InboxKind::Package, &subject, Resolution::Ignore),
            );
            self.commit_manifest(&format!("ignore {name}"));
            Response::Ok {
                message: format!("{name} will never be offered again"),
            }
        }
    }

    /// Bulk onboarding: everything currently installed on request goes
    /// straight into the manifest. Formulae and casks only — a used
    /// Mac's /Applications is too noisy to adopt wholesale; apps stay
    /// offer-driven.
    pub(super) fn pkg_adopt_installed(&mut self) -> Response {
        let mut adopted = 0;
        for (provider, installed) in self.pkg_roots.installed() {
            if provider == Provider::App {
                continue;
            }
            for name in installed {
                soft(self.db.pkg_state_add(provider.as_str(), &name));
                if !self.manifest.contains(provider, &name)
                    && !self.manifest.ignored(provider, &name)
                    && self.manifest.add(provider, &name)
                {
                    adopted += 1;
                }
            }
        }
        if adopted > 0 {
            soft(self.db.record(
                EventKind::PkgAdopted,
                "bulk",
                &format!("{adopted} package(s)"),
            ));
            self.commit_manifest(&format!("adopt {adopted} installed"));
        }
        Response::Ok {
            message: format!("adopted {adopted} package(s)"),
        }
    }

    /// Package inbox items. Approve on an offer adopts; ignore is the
    /// PERMANENT opt-out ("hey, don't track this program"). Approve on
    /// a gone-item drops the manifest entry; ignore keeps it so
    /// `pkg sync` can bring it back.
    pub(super) fn resolve_package(
        &mut self,
        id: i64,
        kind: InboxKind,
        subject: &str,
        resolution: Resolution,
    ) -> Response {
        let Some((provider, name)) = pkg::parse_subject(subject) else {
            return Response::Error {
                message: format!("malformed package subject {subject}"),
            };
        };
        if resolution == Resolution::Redact {
            return Response::Error {
                message: "redact does not apply to packages — approve or ignore".to_string(),
            };
        }
        let name = name.to_string();
        soft(self.db.inbox_resolve(id, resolution));
        soft(
            self.db
                .record(EventKind::Resolved, subject, resolution.as_str()),
        );
        match (kind, resolution) {
            (InboxKind::Package, Resolution::Approve) => {
                self.manifest.add(provider, &name);
                soft(self.db.record(EventKind::PkgAdopted, subject, "from inbox"));
                self.commit_manifest(&format!("+{name}"));
            }
            (InboxKind::Package, Resolution::Ignore) => {
                self.manifest.add_ignore(provider, &name);
                soft(self.db.record(EventKind::PkgIgnored, subject, "from inbox"));
                self.commit_manifest(&format!("ignore {name}"));
            }
            (InboxKind::PackageGone, Resolution::Approve) => {
                self.manifest.remove(provider, &name);
                soft(self.db.record(EventKind::PkgRemoved, subject, "from inbox"));
                self.commit_manifest(&format!("-{name}"));
            }
            _ => {} // gone + ignore: keep the manifest entry
        }
        Response::Ok {
            message: format!("resolved {subject} ({})", resolution.as_str()),
        }
    }
}
