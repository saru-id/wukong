//! The engine's integration suite: a real engine over a tempdir —
//! database, store, fake package roots — driven through the same
//! methods the daemon loop calls.

use super::*;
use wukong_core::events::{EventKind, InboxKind, Resolution};
use wukong_core::ipc::Response;
use wukong_core::pkg::{Manifest, Provider};
use wukong_core::{Config, paths};

/// An engine wired to a tempdir, with a zero debounce so `tick`
/// settles immediately. `_guard` keeps the tempdir alive.
struct Rig {
    engine: Engine,
    home: PathBuf,
    _guard: tempfile::TempDir,
}

fn rig() -> Rig {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut config = Config {
        machine: "testbox".to_string(),
        debounce_secs: 0,
        ..Config::default()
    };
    // Hermetic and fast: no provider detection (which would shell
    // `npm root -g` and read the DEVELOPER's real package worlds).
    config.packages.enabled = false;
    let engine = Engine::new(config, &root.join("wukong.db"), &root.join("store")).unwrap();
    Rig {
        engine,
        home,
        _guard: tmp,
    }
}

fn track(rig: &mut Rig, name: &str, content: &str) -> PathBuf {
    let file = rig.home.join(name);
    std::fs::write(&file, content).unwrap();
    let resp = rig.engine.track(file.to_str().unwrap(), false, false);
    assert!(matches!(resp, Response::Ok { .. }), "{resp:?}");
    file
}

fn edit_and_settle(rig: &mut Rig, file: &Path, content: &str) -> usize {
    std::fs::write(file, content).unwrap();
    rig.engine.touch(file.to_path_buf());
    rig.engine.tick()
}

fn store_content(rig: &Rig, file: &Path) -> Option<String> {
    std::fs::read_to_string(rig.engine.store.dir().join(paths::store_rel(file))).ok()
}

const SECRET: &str = "ghp_abcdefghijklmnopqrstuvwxyz012345";

#[test]
fn clean_edit_commits_with_real_summary() {
    let mut rig = rig();
    let file = track(&mut rig, ".zshrc", "export A=1\n");
    edit_and_settle(&mut rig, &file, "export A=1\nexport B=2\n");
    assert_eq!(
        store_content(&rig, &file).as_deref(),
        Some("export A=1\nexport B=2\n")
    );
    let events = rig.engine.db.events(10).unwrap();
    let commit = events
        .iter()
        .find(|e| e.kind == EventKind::Committed.as_str() && e.detail != "updated")
        .expect("commit with a real summary");
    assert_eq!(commit.detail, "+1 lines");
}

#[test]
fn secret_edit_quarantines_with_masked_body() {
    let mut rig = rig();
    let file = track(&mut rig, ".zshrc", "export A=1\n");
    let new_items = edit_and_settle(&mut rig, &file, &format!("export A=1\nexport T={SECRET}\n"));
    assert_eq!(new_items, 1);
    // Store still has the old content — the secret never landed.
    assert_eq!(store_content(&rig, &file).as_deref(), Some("export A=1\n"));
    // And the inbox evidence is masked.
    let item = &rig.engine.db.inbox_open().unwrap()[0];
    assert!(!item.body.contains(SECRET), "body leaks: {}", item.body);
    assert!(!item.meta.is_empty());
}

#[test]
fn approve_persists_no_requarantine_on_next_edit() {
    let mut rig = rig();
    let file = track(&mut rig, ".zshrc", "export A=1\n");
    edit_and_settle(&mut rig, &file, &format!("export T={SECRET}\n"));
    let item_id = rig.engine.db.inbox_open().unwrap()[0].id;
    rig.engine.resolve(item_id, Resolution::Approve);
    // Approved: committed as-is.
    assert_eq!(
        store_content(&rig, &file).as_deref(),
        Some(&*format!("export T={SECRET}\n"))
    );
    // The same token in a later edit does NOT re-quarantine.
    let new_items = edit_and_settle(&mut rig, &file, &format!("export T={SECRET}\nexport B=2\n"));
    assert_eq!(new_items, 0);
    assert_eq!(rig.engine.db.inbox_count().unwrap(), 0);
    assert!(store_content(&rig, &file).unwrap().contains("export B=2"));
}

#[test]
fn redact_masks_store_leaves_live_alone() {
    let mut rig = rig();
    let file = track(&mut rig, ".zshrc", "export A=1\n");
    let live_content = format!("export T={SECRET}\n");
    edit_and_settle(&mut rig, &file, &live_content);
    let item_id = rig.engine.db.inbox_open().unwrap()[0].id;
    rig.engine.resolve(item_id, Resolution::Redact);
    let stored = store_content(&rig, &file).unwrap();
    assert!(!stored.contains(SECRET), "store leaks: {stored}");
    assert!(stored.contains("ghp_……"), "{stored}");
    // Live file untouched.
    assert_eq!(std::fs::read_to_string(&file).unwrap(), live_content);
    // And the redaction is sticky across future edits.
    let new_items = edit_and_settle(&mut rig, &file, &format!("export T={SECRET}\nexport C=3\n"));
    assert_eq!(new_items, 0);
    let stored = store_content(&rig, &file).unwrap();
    assert!(!stored.contains(SECRET));
    assert!(stored.contains("export C=3"));
}

#[test]
fn approve_does_not_cover_new_secrets() {
    let mut rig = rig();
    let file = track(&mut rig, ".zshrc", "export A=1\n");
    edit_and_settle(&mut rig, &file, &format!("export T={SECRET}\n"));
    let item_id = rig.engine.db.inbox_open().unwrap()[0].id;
    // A second, different secret sneaks in before the approval.
    let rotated = "ghp_zyxwvutsrqponmlkjihgfedcba543210";
    std::fs::write(&file, format!("export T={SECRET}\nexport U={rotated}\n")).unwrap();
    rig.engine.resolve(item_id, Resolution::Approve);
    // The new secret must be quarantined, not committed.
    let stored = store_content(&rig, &file).unwrap_or_default();
    assert!(!stored.contains(rotated), "new secret leaked: {stored}");
    assert_eq!(rig.engine.db.inbox_count().unwrap(), 1);
}

#[test]
fn forbidden_sentinel_changes_are_not_offered() {
    let mut rig = rig();
    let creds = rig.home.join("credentials.json");
    std::fs::write(&creds, "{\"token\": \"whatever\"}").unwrap();
    // Simulate a sentinel-routed settle for an untracked file.
    let rel = paths::store_rel(&creds).to_string_lossy().into_owned();
    let offered = rig.engine.offer_sentinel(&creds, &rel);
    assert_eq!(offered, 0);
    assert_eq!(rig.engine.db.inbox_count().unwrap(), 0);
}

#[test]
fn track_refuses_forbidden() {
    let mut rig = rig();
    let env = rig.home.join(".env");
    std::fs::write(&env, "SECRET=x").unwrap();
    let resp = rig.engine.track(env.to_str().unwrap(), false, false);
    assert!(matches!(resp, Response::Error { .. }), "{resp:?}");
}

/// A rig with package roots pointed at fake trees in the tempdir.
fn pkg_rig() -> Rig {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    for dir in ["brew/Cellar", "brew/Caskroom", "Applications"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    let mut config = Config {
        machine: "testbox".to_string(),
        debounce_secs: 0,
        ..Config::default()
    };
    config.packages.brew_prefix = Some(root.join("brew"));
    config.packages.applications_dir = Some(root.join("Applications"));
    // Hermetic: point every other provider at a nonexistent path so
    // the DEVELOPER's real npm/cargo/go/gem installs can never leak
    // into the test's world.
    for provider in [
        "npm", "pnpm", "bun", "cargo", "go", "gem", "pipx", "uv", "dotnet", "pub",
    ] {
        config.packages.roots.insert(
            provider.to_string(),
            root.join(format!("absent-{provider}")),
        );
    }
    let engine = Engine::new(config, &root.join("wukong.db"), &root.join("store")).unwrap();
    Rig {
        engine,
        home: root.clone(),
        _guard: tmp,
    }
}

fn brew_install(rig: &Rig, name: &str, on_request: bool) {
    let vdir = rig.home.join("brew/Cellar").join(name).join("1.0");
    std::fs::create_dir_all(&vdir).unwrap();
    std::fs::write(
        vdir.join("INSTALL_RECEIPT.json"),
        format!("{{\"installed_on_request\":{on_request}}}"),
    )
    .unwrap();
}

fn brew_uninstall(rig: &Rig, name: &str) {
    std::fs::remove_dir_all(rig.home.join("brew/Cellar").join(name)).unwrap();
}

#[test]
fn first_reconcile_baselines_silently() {
    let mut rig = pkg_rig();
    brew_install(&rig, "jq", true);
    std::fs::create_dir_all(rig.home.join("Applications/Raycast.app")).unwrap();
    assert_eq!(rig.engine.reconcile(), 0);
    assert_eq!(rig.engine.db.inbox_count().unwrap(), 0);
    // …but the state is acknowledged, so nothing re-offers later.
    assert_eq!(rig.engine.reconcile(), 0);
}

#[test]
fn new_install_offers_dependency_does_not() {
    let mut rig = pkg_rig();
    rig.engine.reconcile(); // baseline (empty)
    brew_install(&rig, "ripgrep", true);
    brew_install(&rig, "oniguruma", false); // dependency
    assert_eq!(rig.engine.reconcile(), 1);
    let items = rig.engine.db.inbox_open().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].subject, "formula:ripgrep");
    // Offer fires once per transition, not per reconcile.
    assert_eq!(rig.engine.reconcile(), 0);
}

#[test]
fn adopt_and_permanent_ignore() {
    let mut rig = pkg_rig();
    rig.engine.reconcile();
    brew_install(&rig, "ripgrep", true);
    brew_install(&rig, "htop", true);
    rig.engine.reconcile();
    let items = rig.engine.db.inbox_open().unwrap();
    let rg = items
        .iter()
        .find(|i| i.subject == "formula:ripgrep")
        .unwrap();
    let ht = items.iter().find(|i| i.subject == "formula:htop").unwrap();

    rig.engine.resolve(rg.id, Resolution::Approve);
    assert!(rig.engine.manifest.contains(Provider::Formula, "ripgrep"));

    rig.engine.resolve(ht.id, Resolution::Never);
    assert!(rig.engine.manifest.ignored(Provider::Formula, "htop"));
    // The permanent part: uninstall and reinstall → no new offer.
    brew_uninstall(&rig, "htop");
    rig.engine.reconcile();
    brew_install(&rig, "htop", true);
    assert_eq!(rig.engine.reconcile(), 0);
    assert_eq!(rig.engine.db.inbox_count().unwrap(), 0);
    // And the manifest survives in the store, committed.
    let stored = Manifest::load(rig.engine.store.dir()).unwrap().unwrap();
    assert!(stored.contains(Provider::Formula, "ripgrep"));
    assert!(stored.ignored(Provider::Formula, "htop"));
}

#[test]
fn manifest_member_gone_offers_removal() {
    let mut rig = pkg_rig();
    rig.engine.reconcile();
    brew_install(&rig, "jq", true);
    rig.engine
        .pkg_record(Provider::Formula, "jq", false, false, false);
    rig.engine.reconcile();
    assert_eq!(rig.engine.db.inbox_count().unwrap(), 0);

    brew_uninstall(&rig, "jq");
    assert_eq!(rig.engine.reconcile(), 1);
    let item = &rig.engine.db.inbox_open().unwrap()[0];
    assert_eq!(item.kind(), Some(InboxKind::PackageGone));
    rig.engine.resolve(item.id, Resolution::Approve);
    assert!(!rig.engine.manifest.contains(Provider::Formula, "jq"));
}

#[test]
fn pkg_record_supersedes_open_offer() {
    let mut rig = pkg_rig();
    rig.engine.reconcile();
    brew_install(&rig, "fd", true);
    rig.engine.reconcile();
    assert_eq!(rig.engine.db.inbox_count().unwrap(), 1);
    // The user runs `wukong install fd` after brew already had it.
    rig.engine
        .pkg_record(Provider::Formula, "fd", false, false, false);
    assert_eq!(rig.engine.db.inbox_count().unwrap(), 0);
    assert!(rig.engine.manifest.contains(Provider::Formula, "fd"));
}

#[test]
fn bulk_adopt_takes_brew_not_apps() {
    let mut rig = pkg_rig();
    brew_install(&rig, "jq", true);
    brew_install(&rig, "oniguruma", false);
    std::fs::create_dir_all(rig.home.join("brew/Caskroom/raycast")).unwrap();
    std::fs::create_dir_all(rig.home.join("Applications/Safari.app")).unwrap();
    let Response::Ok { message } = rig.engine.pkg_adopt_installed() else {
        panic!("adopt failed");
    };
    assert!(message.contains("adopted 2"), "{message}");
    assert!(rig.engine.manifest.contains(Provider::Formula, "jq"));
    assert!(rig.engine.manifest.contains(Provider::Cask, "raycast"));
    assert!(!rig.engine.manifest.contains(Provider::App, "Safari"));
}

#[test]
fn restore_skips_wukong_namespace() {
    let mut rig = pkg_rig();
    rig.engine
        .pkg_record(Provider::Formula, "jq", false, false, false);
    let resp = rig.engine.restore(None, false);
    let Response::Ok { message } = resp else {
        panic!("restore failed");
    };
    assert!(message.contains("restored 0"), "{message}");
    assert!(!rig.home.join("home/__wukong__").exists());
    assert!(!paths::home().join("__wukong__").exists());
}

#[test]
fn restore_round_trips_and_tracks() {
    let mut rig = rig();
    let file = track(&mut rig, ".gitconfig", "[user]\n\tname = s\n");
    std::fs::remove_file(&file).unwrap();
    let resp = rig.engine.restore(None, false);
    let Response::Ok { message } = resp else {
        panic!("restore failed");
    };
    assert!(message.contains("restored 1 file(s)"), "{message}");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "[user]\n\tname = s\n"
    );
    assert!(rig.engine.tracked_live.contains(&file));
}

#[test]
fn missing_sentinel_promoted_to_dir_requests_recursive_watch() {
    // A sentinel that doesn't exist at startup classifies as a file;
    // when it materializes as a DIRECTORY, touch() must promote it and
    // ask the loop for a recursive watch — this is the request the
    // daemon must drain on non-client messages too.
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    let watched = root.join("home/appsupport");
    let mut config = Config {
        machine: "testbox".to_string(),
        debounce_secs: 0,
        sentinels: vec![watched.to_string_lossy().into_owned()],
        ..Config::default()
    };
    config.packages.enabled = false;
    let mut engine = Engine::new(config, &root.join("wukong.db"), &root.join("store")).unwrap();
    assert!(engine.sentinel_files.contains(&watched));

    std::fs::create_dir_all(&watched).unwrap();
    engine.touch(watched.clone());
    let requests = engine.drain_watch_requests();
    assert_eq!(requests, vec![(watched.clone(), true)]);
    assert!(engine.sentinel_dirs.contains(&watched));

    // And children are governed from now on: a file inside is offered.
    let inside = watched.join("config.txt");
    std::fs::write(&inside, "hello\n").unwrap();
    engine.touch(inside);
    assert_eq!(engine.tick(), 1);
    assert_eq!(
        engine.db.inbox_open().unwrap()[0].kind(),
        Some(InboxKind::Sentinel)
    );
}

#[test]
fn observe_only_acknowledges_without_manifest() {
    let mut rig = pkg_rig();
    rig.engine.reconcile();
    brew_install(&rig, "jq", true);
    // `wukong install --no-track jq`: reality acknowledged, manifest
    // untouched, and no later adoption offer for it.
    rig.engine
        .pkg_record(Provider::Formula, "jq", false, false, true);
    assert!(!rig.engine.manifest.contains(Provider::Formula, "jq"));
    assert_eq!(rig.engine.reconcile(), 0);
    assert_eq!(rig.engine.db.inbox_count().unwrap(), 0);
}

#[test]
fn reverse_transition_auto_resolves_stale_offer() {
    let mut rig = pkg_rig();
    rig.engine.reconcile();
    brew_install(&rig, "fd", true);
    rig.engine.reconcile();
    assert_eq!(rig.engine.db.inbox_count().unwrap(), 1); // adopt offer
    // Uninstalled before the user ever resolved it: the offer must not
    // linger — approving a ghost would adopt a missing package.
    brew_uninstall(&rig, "fd");
    rig.engine.reconcile();
    assert_eq!(rig.engine.db.inbox_count().unwrap(), 0);
}

#[test]
fn oversized_tracked_file_never_commits() {
    let mut rig = rig();
    let file = track(&mut rig, ".zshrc", "export A=1\n");
    let huge = "x".repeat(MAX_TRACKED_BYTES + 1);
    assert_eq!(edit_and_settle(&mut rig, &file, &huge), 0);
    // The mirror still holds the last good content.
    assert_eq!(store_content(&rig, &file).as_deref(), Some("export A=1\n"));
}

#[test]
fn poisoned_manifest_blocks_offers_and_saves() {
    let rig = pkg_rig();
    // Corrupt the on-disk manifest, then rebuild the engine over it.
    let store_dir = rig.engine.store.dir().to_path_buf();
    let manifest_path = store_dir.join(wukong_core::pkg::MANIFEST_REL);
    std::fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
    std::fs::write(&manifest_path, "this is [not toml").unwrap();
    let db_path = rig.home.join("wukong2.db");
    let mut engine = Engine::new(rig.engine.config.clone(), &db_path, &store_dir).unwrap();

    // New package appears: no offer while poisoned, transition kept.
    brew_install(&rig, "ripgrep", true);
    assert_eq!(engine.reconcile(), 0);
    assert_eq!(engine.db.inbox_count().unwrap(), 0);
    // And a record attempt must not clobber the unparseable file.
    engine.pkg_record(Provider::Formula, "ripgrep", false, false, false);
    assert_eq!(
        std::fs::read_to_string(&manifest_path).unwrap(),
        "this is [not toml"
    );
}

#[test]
fn local_only_config_reports_zero_unpushed() {
    let mut rig = rig(); // no remote configured
    let file = track(&mut rig, ".zshrc", "export A=1\n");
    edit_and_settle(&mut rig, &file, "export A=1\nexport B=2\n");
    let Response::Status(status) = rig.engine.status() else {
        panic!("expected status");
    };
    assert_eq!(status.unpushed, 0, "local-only must not count unpushed");
    assert!(!rig.engine.wants_push());
}

#[test]
fn exclude_silences_a_subtree_and_resolves_open_offers() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("home");
    let noisy = home.join("config-tree/noisyapp");
    std::fs::create_dir_all(&noisy).unwrap();
    let mut config = Config {
        machine: "testbox".to_string(),
        debounce_secs: 0,
        sentinels: vec![home.join("config-tree").to_string_lossy().into_owned()],
        ..Config::default()
    };
    config.packages.enabled = false;
    let mut engine = Engine::new(config, &root.join("wukong.db"), &root.join("store")).unwrap();

    // An offer opens for a file under the noisy subtree.
    let file = noisy.join("state.json");
    std::fs::write(&file, "{}").unwrap();
    engine.touch(file.clone());
    assert_eq!(engine.tick(), 1);

    // Exclude the subtree: the open offer resolves, and further churn
    // is silent.
    let resp = engine.exclude(noisy.to_str().unwrap());
    assert!(matches!(resp, Response::Ok { .. }));
    assert_eq!(engine.db.inbox_count().unwrap(), 0);
    std::fs::write(noisy.join("more.json"), "{}").unwrap();
    engine.touch(noisy.join("more.json"));
    assert_eq!(engine.tick(), 0);
    // In-memory config gained the entry (persistence needs an on-disk
    // source, which a test config deliberately lacks).
    assert!(engine.config.exclude.iter().any(|e| e.contains("noisyapp")));
    // Tracking still outranks excluding.
    let tracked = home.join("config-tree/noisyapp/keep.conf");
    std::fs::write(&tracked, "keep=1\n").unwrap();
    let resp = engine.track(tracked.to_str().unwrap(), false, false);
    assert!(matches!(resp, Response::Ok { .. }), "{resp:?}");
    std::fs::write(&tracked, "keep=2\n").unwrap();
    engine.touch(tracked.clone());
    engine.tick();
    let stored =
        std::fs::read_to_string(engine.store.dir().join(paths::store_rel(&tracked))).unwrap();
    assert_eq!(stored, "keep=2\n");
}

#[test]
fn diff_and_log_answer_the_daily_questions() {
    let mut rig = rig();
    let file = track(&mut rig, ".zshrc", "export A=1\n");
    edit_and_settle(&mut rig, &file, "export A=1\nexport B=2\n");

    // Unsettled live edit shows up in diff.
    std::fs::write(&file, "export A=1\nexport B=2\nexport C=3\n").unwrap();
    let Response::Ok { message } = rig.engine.diff(file.to_str().unwrap()) else {
        panic!("diff failed");
    };
    assert!(message.contains("+export C=3"), "{message}");

    // History lists both commits, newest first.
    let Response::Ok { message } = rig.engine.file_log(file.to_str().unwrap(), 10) else {
        panic!("log failed");
    };
    assert_eq!(message.lines().count(), 2, "{message}");
    assert!(message.contains("+1 lines"), "{message}");

    // Untracked files are refused.
    let other = rig.home.join(".other");
    std::fs::write(&other, "x").unwrap();
    assert!(matches!(
        rig.engine.diff(other.to_str().unwrap()),
        Response::Error { .. }
    ));
}

#[test]
fn last_push_survives_a_daemon_restart() {
    let mut rig = rig();
    rig.engine
        .db
        .record(EventKind::Pushed, "testbox", "")
        .unwrap();
    rig.engine.last_push = None; // simulate a fresh boot's in-memory state
    let Response::Status(status) = rig.engine.status() else {
        panic!("expected status");
    };
    assert!(
        status.last_push.is_some(),
        "should fall back to the event log"
    );
}

/// A rig with settings governance pointed at a fake preferences dir.
fn settings_rig() -> (Rig, PathBuf) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("home");
    let prefs = root.join("prefs");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&prefs).unwrap();
    let mut config = Config {
        machine: "testbox".to_string(),
        debounce_secs: 0,
        ..Config::default()
    };
    config.packages.enabled = false;
    config.settings.preferences_dir = Some(prefs.clone());
    let engine = Engine::new(config, &root.join("wukong.db"), &root.join("store")).unwrap();
    (
        Rig {
            engine,
            home,
            _guard: tmp,
        },
        prefs,
    )
}

fn write_pref(prefs: &Path, domain: &str, key: &str, value: plist::Value) {
    use wukong_core::settings::plist_path;
    let path = plist_path(prefs, domain);
    let mut dict = match plist::Value::from_file(&path) {
        Ok(plist::Value::Dictionary(d)) => d,
        _ => plist::Dictionary::new(),
    };
    dict.insert(key.to_string(), value);
    plist::Value::Dictionary(dict)
        .to_file_binary(&path)
        .unwrap();
}

#[test]
fn settings_baseline_then_change_offers_and_approve_records() {
    let (mut rig, prefs) = settings_rig();
    // Pre-existing tuned value: baselined silently.
    write_pref(
        &prefs,
        "com.apple.dock",
        "autohide",
        plist::Value::Boolean(true),
    );
    assert_eq!(rig.engine.reconcile_settings(), 0);
    assert_eq!(rig.engine.reconcile_settings(), 0); // marker holds

    // The value changes: one offer, once.
    write_pref(
        &prefs,
        "com.apple.dock",
        "autohide",
        plist::Value::Boolean(false),
    );
    assert_eq!(rig.engine.reconcile_settings(), 1);
    assert_eq!(rig.engine.reconcile_settings(), 0);
    let item = &rig.engine.db.inbox_open().unwrap()[0];
    assert_eq!(item.kind(), Some(InboxKind::Setting));
    assert!(item.body.contains("true → false") || item.body.contains("(unset) → false"));

    // Approve: recorded in the manifest, committed under the banner.
    rig.engine.resolve(item.id, Resolution::Approve);
    let desired = rig
        .engine
        .settings_manifest
        .desired("com.apple.dock", "autohide")
        .cloned();
    assert!(matches!(
        desired,
        Some(wukong_core::settings::Value::Bool(false))
    ));
    let log = rig
        .engine
        .store
        .log(Path::new(wukong_core::settings::MANIFEST_REL), 5)
        .unwrap();
    assert!(
        log.contains("settings: com.apple.dock autohide = false"),
        "{log}"
    );
}

#[test]
fn settings_manifest_match_is_silent_and_ignore_is_permanent() {
    let (mut rig, prefs) = settings_rig();
    rig.engine.reconcile_settings(); // baseline (empty)

    // A change lands that ALREADY matches the manifest (sync applied,
    // or the user set the desired value by hand): silence.
    rig.engine.settings_manifest.set(
        "com.apple.finder",
        "ShowPathbar",
        wukong_core::settings::Value::Bool(true),
    );
    write_pref(
        &prefs,
        "com.apple.finder",
        "ShowPathbar",
        plist::Value::Boolean(true),
    );
    assert_eq!(rig.engine.reconcile_settings(), 0);
    // Bool/Int coercion counts as a match too (macOS writes both).
    write_pref(
        &prefs,
        "com.apple.finder",
        "ShowPathbar",
        plist::Value::Integer(1.into()),
    );
    assert_eq!(rig.engine.reconcile_settings(), 0);

    // Drift away from the desired value opens an offer; returning to
    // it (sync, or a manual revert) auto-resolves the stale offer.
    write_pref(
        &prefs,
        "com.apple.finder",
        "ShowPathbar",
        plist::Value::Boolean(false),
    );
    assert_eq!(rig.engine.reconcile_settings(), 1);
    write_pref(
        &prefs,
        "com.apple.finder",
        "ShowPathbar",
        plist::Value::Boolean(true),
    );
    assert_eq!(rig.engine.reconcile_settings(), 0);
    assert_eq!(rig.engine.db.inbox_count().unwrap(), 0);

    // Ignore is the permanent opt-out.
    write_pref(
        &prefs,
        "NSGlobalDomain",
        "KeyRepeat",
        plist::Value::Integer(2.into()),
    );
    assert_eq!(rig.engine.reconcile_settings(), 1);
    let item_id = rig.engine.db.inbox_open().unwrap()[0].id;
    rig.engine.resolve(item_id, Resolution::Never);
    write_pref(
        &prefs,
        "NSGlobalDomain",
        "KeyRepeat",
        plist::Value::Integer(6.into()),
    );
    assert_eq!(rig.engine.reconcile_settings(), 0);
    assert_eq!(rig.engine.db.inbox_count().unwrap(), 0);
}

#[test]
fn settings_record_takes_arbitrary_keys_and_list_reports_drift() {
    let (mut rig, prefs) = settings_rig();
    rig.engine.reconcile_settings();
    // A key far outside the corpus.
    write_pref(
        &prefs,
        "org.custom.tool",
        "fancyMode",
        plist::Value::String("on".into()),
    );
    let resp = rig.engine.settings_record("org.custom.tool", "fancyMode");
    assert!(matches!(resp, Response::Ok { .. }), "{resp:?}");

    // Drift: live changes away from the recorded value.
    write_pref(
        &prefs,
        "org.custom.tool",
        "fancyMode",
        plist::Value::String("off".into()),
    );
    let Response::Settings { entries, .. } = rig.engine.settings_list() else {
        panic!("expected settings");
    };
    let entry = entries
        .iter()
        .find(|e| e.domain == "org.custom.tool")
        .unwrap();
    assert!(!entry.in_sync);
    assert!(entry.label.is_none()); // outside the corpus
    // Corpus entries carry labels.
    assert!(
        entries
            .iter()
            .any(|e| e.domain == "com.apple.dock" && e.label.is_some())
    );
    // Recording an unset key is refused with a real message.
    let resp = rig.engine.settings_record("org.custom.tool", "absent");
    assert!(matches!(resp, Response::Error { .. }));
}

#[test]
fn capture_diffs_signal_from_noise_and_is_one_shot() {
    let (mut rig, prefs) = settings_rig();
    write_pref(
        &prefs,
        "com.apple.dock",
        "autohide",
        plist::Value::Boolean(true),
    );
    write_pref(
        &prefs,
        "org.some.app",
        "steady",
        plist::Value::Integer(7.into()),
    );

    // No capture in progress → a real error, not an empty diff.
    assert!(matches!(rig.engine.capture_diff(), Response::Error { .. }));

    assert!(matches!(rig.engine.capture_start(), Response::Ok { .. }));
    // One real change, one noise change, one removal, one untouched.
    write_pref(
        &prefs,
        "com.apple.dock",
        "autohide",
        plist::Value::Boolean(false),
    );
    write_pref(
        &prefs,
        "org.some.app",
        "NSWindow Frame main",
        plist::Value::String("0 0 100 100".into()),
    );

    let Response::CaptureDiff { changes } = rig.engine.capture_diff() else {
        panic!("expected diff");
    };
    let signal: Vec<_> = changes.iter().filter(|c| !c.noise).collect();
    assert_eq!(signal.len(), 1, "{changes:#?}");
    assert_eq!(signal[0].key, "autohide");
    assert!(signal[0].label.is_some(), "corpus key carries its label");
    assert!(
        changes
            .iter()
            .any(|c| c.noise && c.key.contains("NSWindow"))
    );
    // Untouched keys never appear.
    assert!(!changes.iter().any(|c| c.key == "steady"));
    // Signal sorts before noise.
    assert!(!changes[0].noise);

    // The snapshot was consumed: a second diff errors.
    assert!(matches!(rig.engine.capture_diff(), Response::Error { .. }));
}

#[test]
fn capture_then_record_makes_the_key_governed() {
    let (mut rig, prefs) = settings_rig();
    rig.engine.reconcile_settings(); // baseline
    rig.engine.capture_start();
    write_pref(
        &prefs,
        "org.custom.tool",
        "fancyMode",
        plist::Value::String("on".into()),
    );
    let Response::CaptureDiff { changes } = rig.engine.capture_diff() else {
        panic!("expected diff");
    };
    assert_eq!(changes.len(), 1);

    // Record it, exactly as the interactive picker would.
    let resp = rig.engine.settings_record("org.custom.tool", "fancyMode");
    assert!(matches!(resp, Response::Ok { .. }));
    // Governed from now on: a later change offers ambiently.
    write_pref(
        &prefs,
        "org.custom.tool",
        "fancyMode",
        plist::Value::String("off".into()),
    );
    assert_eq!(rig.engine.reconcile_settings(), 1);
}

/// A rig whose seal identity lives in a tempdir file (no Keychain).
fn seal_rig() -> Rig {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut config = Config {
        machine: "testbox".to_string(),
        debounce_secs: 0,
        ..Config::default()
    };
    config.packages.enabled = false;
    config.seal.identity_file = Some(root.join("age.key"));
    let engine = Engine::new(config, &root.join("wukong.db"), &root.join("store")).unwrap();
    Rig {
        engine,
        home,
        _guard: tmp,
    }
}

const SEALED_SECRET: &str = "ghp_sealedsealedsealedsealedsealed01";

#[test]
fn sealed_track_stores_ciphertext_only_with_hash_guard() {
    let mut rig = seal_rig();
    let file = rig.home.join(".env");
    std::fs::write(&file, format!("TOKEN={SEALED_SECRET}\n")).unwrap();

    // .env is forbidden plaintext — but sealed tracking is the point.
    assert!(matches!(
        rig.engine.track(file.to_str().unwrap(), false, false),
        Response::Error { .. }
    ));
    let resp = rig.engine.track(file.to_str().unwrap(), true, false);
    assert!(matches!(resp, Response::Ok { .. }), "{resp:?}");

    // The store holds age ciphertext, never the plaintext.
    let stored = std::fs::read(rig.engine.store.dir().join(paths::store_rel(&file))).unwrap();
    assert!(wukong_core::seal::is_sealed(&stored));
    assert!(
        !stored
            .windows(SEALED_SECRET.len())
            .any(|w| w == SEALED_SECRET.as_bytes())
    );
    // The recipient landed in the store; the identity did NOT.
    assert!(
        rig.engine
            .store
            .dir()
            .join(wukong_core::seal::RECIPIENT_REL)
            .exists()
    );
    assert!(
        !rig.engine.store.dir().join("age.key").exists()
            && !rig
                .engine
                .store
                .files()
                .unwrap()
                .iter()
                .any(|f| f.to_string_lossy().contains("age.key"))
    );

    // Determinism guard: an unchanged file must not re-commit
    // (ciphertext would differ every time).
    let before = rig
        .engine
        .store
        .log(&paths::store_rel(&file), 10)
        .unwrap()
        .lines()
        .count();
    rig.engine.touch(file.clone());
    rig.engine.tick();
    let after = rig
        .engine
        .store
        .log(&paths::store_rel(&file), 10)
        .unwrap()
        .lines()
        .count();
    assert_eq!(before, after, "unchanged sealed file re-committed");

    // A real edit commits (as ciphertext) — and never quarantines.
    std::fs::write(&file, format!("TOKEN={SEALED_SECRET}\nMORE=1\n")).unwrap();
    rig.engine.touch(file.clone());
    assert_eq!(rig.engine.tick(), 0);
    assert_eq!(
        rig.engine
            .store
            .log(&paths::store_rel(&file), 10)
            .unwrap()
            .lines()
            .count(),
        after + 1
    );
}

#[test]
fn restore_decrypts_sealed_files_and_keeps_them_sealed() {
    let mut rig = seal_rig();
    let file = rig.home.join(".env");
    let plaintext = format!("TOKEN={SEALED_SECRET}\n");
    std::fs::write(&file, &plaintext).unwrap();
    rig.engine.track(file.to_str().unwrap(), true, false);

    std::fs::remove_file(&file).unwrap();
    // Removing tracked state simulates a fresh machine with the store
    // and the key, but no roster.
    rig.engine.tracked_live.clear();
    rig.engine.sealed_live.clear();
    let resp = rig.engine.restore(Some(file.to_str().unwrap()), false);
    assert!(matches!(resp, Response::Ok { .. }), "{resp:?}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), plaintext);
    assert!(rig.engine.sealed_live.contains(&file));
}

#[test]
fn quarantine_seal_resolution_moves_the_file_to_the_sealed_lane() {
    let mut rig = seal_rig();
    let file = track(&mut rig, ".zshrc", "export A=1\n");
    edit_and_settle(&mut rig, &file, &format!("export T={SEALED_SECRET}\n"));
    let item_id = rig.engine.db.inbox_open().unwrap()[0].id;
    rig.engine.resolve(item_id, Resolution::Seal);

    let stored = std::fs::read(rig.engine.store.dir().join(paths::store_rel(&file))).unwrap();
    assert!(wukong_core::seal::is_sealed(&stored));
    // Sticky: further edits stay ciphertext, no re-quarantine.
    let n = edit_and_settle(&mut rig, &file, &format!("export T={SEALED_SECRET}\nB=2\n"));
    assert_eq!(n, 0);
    assert_eq!(rig.engine.db.inbox_count().unwrap(), 0);
    let stored = std::fs::read(rig.engine.store.dir().join(paths::store_rel(&file))).unwrap();
    assert!(wukong_core::seal::is_sealed(&stored));
}

#[test]
fn unseal_goes_back_through_the_gate() {
    let mut rig = seal_rig();
    let file = rig.home.join(".sealed-config");
    std::fs::write(&file, format!("export T={SEALED_SECRET}\n")).unwrap();
    rig.engine.track(file.to_str().unwrap(), true, false);
    let resp = rig.engine.unseal(file.to_str().unwrap());
    assert!(matches!(resp, Response::Ok { .. }));
    // The secret is now HELD — plaintext did not slip into the store.
    assert_eq!(rig.engine.db.inbox_count().unwrap(), 1);
    let stored = std::fs::read(rig.engine.store.dir().join(paths::store_rel(&file))).unwrap();
    assert!(
        wukong_core::seal::is_sealed(&stored),
        "store must still hold the last sealed blob, not plaintext"
    );
}

#[test]
fn language_providers_offer_and_adopt() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    let npm = root.join("npmroot");
    std::fs::create_dir_all(npm.join("typescript")).unwrap();
    std::fs::create_dir_all(npm.join("@biomejs/biome")).unwrap();
    std::fs::create_dir_all(npm.join(".bin")).unwrap();
    let cargo = root.join("cargohome");
    std::fs::create_dir_all(&cargo).unwrap();
    std::fs::write(
        cargo.join(".crates.toml"),
        "[v1]\n\"ripgrep 14.1.0 (registry+https://github.com/rust-lang/crates.io-index)\" = [\"rg\"]\n",
    )
    .unwrap();

    let mut config = Config {
        machine: "testbox".to_string(),
        debounce_secs: 0,
        ..Config::default()
    };
    for provider in [
        "formula", "cask", "app", "pnpm", "bun", "go", "gem", "pipx", "uv", "dotnet", "pub",
    ] {
        config.packages.roots.insert(
            provider.to_string(),
            root.join(format!("absent-{provider}")),
        );
    }
    config.packages.roots.insert("npm".to_string(), npm.clone());
    config.packages.roots.insert("cargo".to_string(), cargo);
    let mut engine = Engine::new(config, &root.join("wukong.db"), &root.join("store")).unwrap();

    engine.reconcile(); // baseline swallows the pre-existing world
    std::fs::create_dir_all(npm.join("prettier")).unwrap();
    assert_eq!(engine.reconcile(), 1);
    let item = &engine.db.inbox_open().unwrap()[0];
    assert_eq!(item.subject, "npm:prettier");
    engine.resolve(item.id, Resolution::Approve);
    assert!(engine.manifest.contains(Provider::Npm, "prettier"));
    // Scoped names and cargo installs were baselined as full names.
    let states = engine.db.pkg_state("npm").unwrap();
    assert!(states.contains("@biomejs/biome"));
    assert!(!states.contains(".bin"));
    assert!(engine.db.pkg_state("cargo").unwrap().contains("ripgrep"));
}

/// A sandbox where every expanded provider has a live root — go, gem,
/// dotnet, pub, and an Applications dir holding one drag-install and
/// one App Store app — already baselined.
fn expanded_rig() -> (tempfile::TempDir, std::path::PathBuf, Engine) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    for dir in [
        "gobin",
        "gemhome/specifications",
        "dotnetstore",
        "pubcache",
        "Applications/Dragged.app",
        "Applications/Bought.app/Contents/_MASReceipt",
    ] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    std::fs::write(
        root.join("Applications/Bought.app/Contents/_MASReceipt/receipt"),
        b"apple",
    )
    .unwrap();

    let mut config = Config {
        machine: "testbox".to_string(),
        debounce_secs: 0,
        ..Config::default()
    };
    for provider in [
        "formula", "cask", "npm", "pnpm", "bun", "cargo", "pipx", "uv",
    ] {
        config.packages.roots.insert(
            provider.to_string(),
            root.join(format!("absent-{provider}")),
        );
    }
    config.packages.applications_dir = Some(root.join("Applications"));
    for (provider, dir) in [
        ("go", "gobin"),
        ("gem", "gemhome"),
        ("dotnet", "dotnetstore"),
        ("pub", "pubcache"),
    ] {
        config
            .packages
            .roots
            .insert(provider.to_string(), root.join(dir));
    }
    let mut engine = Engine::new(config, &root.join("wukong.db"), &root.join("store")).unwrap();
    engine.reconcile(); // baseline swallows the pre-existing apps
    assert_eq!(engine.db.inbox_count().unwrap(), 0);
    (tmp, root, engine)
}

#[test]
fn expanded_providers_offer_by_receipt() {
    let (_guard, root, mut engine) = expanded_rig();
    let apps = root.join("Applications");

    // A receipt lands in every new lane at once.
    std::fs::write(
        root.join("gobin/tool"),
        wukong_core::gobuild::synthesize("github.com/x/tool", "v1.2.3"),
    )
    .unwrap();
    std::fs::write(
        root.join("gemhome/specifications/colorls-1.4.6.gemspec"),
        b"Gem::Specification",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("dotnetstore/cake/4.0.0")).unwrap();
    std::fs::create_dir_all(root.join("pubcache/melos")).unwrap();
    std::fs::create_dir_all(apps.join("NewBuy.app/Contents/_MASReceipt")).unwrap();
    std::fs::write(
        apps.join("NewBuy.app/Contents/_MASReceipt/receipt"),
        b"apple",
    )
    .unwrap();
    assert_eq!(engine.reconcile(), 5);
    let items = engine.db.inbox_open().unwrap();
    let subjects: Vec<&str> = items.iter().map(|i| i.subject.as_str()).collect();
    for expected in [
        "go:github.com/x/tool",
        "gem:colorls",
        "dotnet:cake",
        "pub:melos",
        "mas:NewBuy",
    ] {
        assert!(subjects.contains(&expected), "{subjects:?}");
    }

    // Adoption works for module-path names, and a fake App Store app
    // adopts WITHOUT an id (Spotlight knows nothing about it).
    for item in &items {
        engine.resolve(item.id, Resolution::Approve);
    }
    assert!(engine.manifest.contains(Provider::Go, "github.com/x/tool"));
    assert!(engine.manifest.contains(Provider::Mas, "NewBuy"));
    assert_eq!(engine.manifest.id_of(Provider::Mas, "NewBuy"), None);

    // pkg_list carries the receipt versions.
    let Response::Packages { entries } = engine.pkg_list() else {
        panic!("expected package list");
    };
    let gem = entries.iter().find(|e| e.name == "colorls").unwrap();
    assert_eq!(gem.version.as_deref(), Some("1.4.6"));
    let tool = entries
        .iter()
        .find(|e| e.name == "github.com/x/tool")
        .unwrap();
    assert_eq!(tool.version.as_deref(), Some("v1.2.3"));

    // The providers table reports every provider, mas riding apps.
    let Response::Providers { entries } = engine.pkg_providers() else {
        panic!("expected provider table");
    };
    assert_eq!(entries.len(), 14);
    let mas = entries
        .iter()
        .find(|e| e.provider == Provider::Mas)
        .unwrap();
    assert!(mas.active);
    assert_eq!(mas.path.as_deref(), Some(apps.to_str().unwrap()));
    assert_eq!(mas.count, Some(2));
    let npm = entries
        .iter()
        .find(|e| e.provider == Provider::Npm)
        .unwrap();
    assert!(!npm.active, "absent override must disable");
}

#[test]
fn skip_is_harmless_never_is_permanent() {
    let mut rig = pkg_rig();
    rig.engine.reconcile(); // baseline
    brew_install(&rig, "htop", true);
    assert_eq!(rig.engine.reconcile(), 1);
    let id = rig.engine.db.inbox_open().unwrap()[0].id;

    // Skip: the offer closes, nothing lands anywhere permanent.
    rig.engine.resolve(id, Resolution::Skip);
    assert!(!rig.engine.manifest.ignored(Provider::Formula, "htop"));
    assert!(!rig.engine.manifest.contains(Provider::Formula, "htop"));
    assert_eq!(rig.engine.db.inbox_count().unwrap(), 0);
    // …and it does not nag: reality is acknowledged, so no re-offer.
    assert_eq!(rig.engine.reconcile(), 0);

    // A fresh transition re-offers; never on the gone-item drops AND
    // ignores in one move.
    brew_uninstall(&rig, "htop");
    rig.engine.reconcile();
    brew_install(&rig, "htop", true);
    assert_eq!(rig.engine.reconcile(), 1);
    let id = rig.engine.db.inbox_open().unwrap()[0].id;
    rig.engine.resolve(id, Resolution::Approve);
    brew_uninstall(&rig, "htop");
    assert_eq!(rig.engine.reconcile(), 1);
    let gone = rig.engine.db.inbox_open().unwrap()[0].id;
    rig.engine.resolve(gone, Resolution::Never);
    assert!(!rig.engine.manifest.contains(Provider::Formula, "htop"));
    assert!(rig.engine.manifest.ignored(Provider::Formula, "htop"));
}

#[test]
fn sentinel_never_excludes_and_quarantine_rejects_never() {
    let mut rig = rig();
    // A sentinel change lands in the inbox…
    let zshrc = rig.home.join(".zshrc");
    std::fs::write(&zshrc, "export A=1\n").unwrap();
    let rel = paths::store_rel(&zshrc).to_string_lossy().into_owned();
    assert_eq!(rig.engine.offer_sentinel(&zshrc, &rel), 1);
    let items = rig.engine.db.inbox_open().unwrap();
    // …and never excludes the path: one word, the same meaning as
    // `wukong exclude`, and the item resolves with it.
    let resp = rig.engine.resolve(items[0].id, Resolution::Never);
    assert!(matches!(resp, Response::Ok { .. }), "{resp:?}");
    assert_eq!(rig.engine.db.inbox_count().unwrap(), 0);
    assert!(
        rig.engine.excludes.contains(&zshrc),
        "{:?}",
        rig.engine.excludes
    );

    // A quarantined secret cannot be waved off forever.
    let notes = rig.home.join("notes.txt");
    std::fs::write(&notes, "ok\n").unwrap();
    rig.engine.track(notes.to_str().unwrap(), false, false);
    std::fs::write(
        &notes,
        "aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n",
    )
    .unwrap();
    rig.engine.touch(notes.clone());
    rig.engine.tick();
    let q = rig
        .engine
        .db
        .inbox_open()
        .unwrap()
        .into_iter()
        .find(|i| i.kind() == Some(InboxKind::Quarantine))
        .expect("quarantined");
    let resp = rig.engine.resolve(q.id, Resolution::Never);
    assert!(matches!(resp, Response::Error { .. }), "{resp:?}");
}

#[test]
fn shared_lane_moves_files_both_ways() {
    let mut rig = rig();
    let file = track(&mut rig, ".gitconfig", "[user]\n\tname = s\n");
    let rel = paths::store_rel(&file);
    assert!(rig.engine.store.dir().join(&rel).is_file());

    // Promote: the mirror moves to the shared branch.
    let resp = rig.engine.share(file.to_str().unwrap(), false);
    assert!(matches!(resp, Response::Ok { .. }), "{resp:?}");
    let shared = rig.engine.store.shared();
    assert!(!rig.engine.store.dir().join(&rel).is_file());
    assert!(shared.dir().join(&rel).is_file());

    // Edits now commit to the shared branch.
    edit_and_settle(&mut rig, &file, "[user]\n\tname = t\n");
    assert_eq!(
        std::fs::read_to_string(shared.dir().join(&rel)).unwrap(),
        "[user]\n\tname = t\n"
    );

    // Restore finds the shared file and re-marks it shared.
    std::fs::remove_file(&file).unwrap();
    rig.engine.restore(Some(file.to_str().unwrap()), false);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "[user]\n\tname = t\n"
    );
    let roster = rig.engine.db.tracked().unwrap();
    assert!(
        roster
            .iter()
            .any(|(r, _, shared)| r.ends_with(".gitconfig") && *shared),
        "{roster:?}"
    );

    // Undo brings it home.
    let resp = rig.engine.share(file.to_str().unwrap(), true);
    assert!(matches!(resp, Response::Ok { .. }), "{resp:?}");
    assert!(rig.engine.store.dir().join(&rel).is_file());
    assert!(!shared.dir().join(&rel).is_file());
}

#[test]
fn shared_manifest_counts_as_wanted_and_moves() {
    let mut rig = pkg_rig();
    rig.engine.reconcile(); // baseline
    // A shared install is wanted everywhere…
    rig.engine
        .pkg_record(Provider::Formula, "jq", false, true, false);
    assert!(rig.engine.shared_manifest.contains(Provider::Formula, "jq"));
    assert!(!rig.engine.manifest.contains(Provider::Formula, "jq"));
    // …so when it appears on disk there is NO adopt offer.
    brew_install(&rig, "jq", true);
    assert_eq!(rig.engine.reconcile(), 0);
    let Response::Packages { entries } = rig.engine.pkg_list() else {
        panic!("expected package list");
    };
    let jq = entries.iter().find(|e| e.name == "jq").unwrap();
    assert!(jq.shared && jq.installed);

    // Lane moves carry the entry (and rm drops it from wherever).
    rig.engine.pkg_share(Provider::Formula, "jq", true);
    assert!(rig.engine.manifest.contains(Provider::Formula, "jq"));
    assert!(!rig.engine.shared_manifest.contains(Provider::Formula, "jq"));
    rig.engine.pkg_share(Provider::Formula, "jq", false);
    rig.engine
        .pkg_record(Provider::Formula, "jq", true, false, false);
    assert!(!rig.engine.shared_manifest.contains(Provider::Formula, "jq"));
}

#[test]
fn shared_settings_fill_in_behind_machine_values() {
    let (mut rig, prefs) = settings_rig();
    write_pref(
        &prefs,
        "com.apple.dock",
        "autohide",
        plist::Value::Boolean(true),
    );
    rig.engine.reconcile_settings(); // baseline
    let resp = rig.engine.settings_record("com.apple.dock", "autohide");
    assert!(matches!(resp, Response::Ok { .. }), "{resp:?}");

    // Promote to shared: the machine manifest empties, the union
    // still wants it — a matching change stays quiet.
    let resp = rig
        .engine
        .setting_share("com.apple.dock", "autohide", false);
    assert!(matches!(resp, Response::Ok { .. }), "{resp:?}");
    assert!(
        rig.engine
            .settings_manifest
            .desired("com.apple.dock", "autohide")
            .is_none()
    );
    assert!(
        rig.engine
            .shared_settings
            .desired("com.apple.dock", "autohide")
            .is_some()
    );
    write_pref(
        &prefs,
        "com.apple.dock",
        "autohide",
        plist::Value::Boolean(false),
    );
    assert_eq!(
        rig.engine.reconcile_settings(),
        1,
        "drift from a shared value still offers"
    );
}

#[test]
fn revert_rewinds_live_and_commits_forward() {
    let mut rig = rig();
    let file = track(&mut rig, ".zshrc", "version one\n");
    edit_and_settle(&mut rig, &file, "version two\n");

    let resp = rig.engine.revert(file.to_str().unwrap(), None);
    assert!(matches!(resp, Response::Ok { .. }), "{resp:?}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "version one\n");
    // The rewind commits through the normal flow — history grows.
    rig.engine.tick();
    assert_eq!(store_content(&rig, &file).as_deref(), Some("version one\n"));
    let rel = paths::store_rel(&file);
    let log = rig.engine.store.log(&rel, 10).unwrap();
    assert_eq!(log.lines().count(), 3, "{log}");

    // A gated revert still gates: reverting TO a version with a secret
    // quarantines rather than committing.
    edit_and_settle(&mut rig, &file, &format!("token {SECRET}\n"));
    let q = rig.engine.db.inbox_open().unwrap();
    assert_eq!(q.len(), 1, "the secret edit quarantined");
}

#[test]
fn health_alerts_on_stale_pushes_and_answers() {
    let mut rig = rig();
    rig.engine.config.remote = "ssh://nowhere/store.git".to_string();
    rig.engine.unpushed = 3;
    soft(
        rig.engine
            .db
            .record(EventKind::PushFailed, "testbox", "no route to host"),
    );
    // Direct call — the hourly gate is bypassed by calling the check.
    let new = rig.engine.check_push_health();
    assert_eq!(new, 1);
    let items = rig.engine.db.inbox_open().unwrap();
    assert_eq!(items[0].subject, "push");
    assert!(items[0].body.contains("no route to host"));
    // Re-checking within the re-alert window stays quiet.
    assert_eq!(rig.engine.check_push_health(), 0);

    // approve queues a push; never is rejected.
    let resp = rig.engine.resolve(items[0].id, Resolution::Never);
    assert!(matches!(resp, Response::Error { .. }), "{resp:?}");
    let resp = rig.engine.resolve(items[0].id, Resolution::Approve);
    assert!(matches!(resp, Response::Ok { .. }), "{resp:?}");
    assert!(rig.engine.wants_push());
}

#[test]
fn editing_the_secret_away_closes_the_stale_quarantine() {
    let mut rig = rig();
    let file = track(&mut rig, ".zshrc", "export A=1\n");
    edit_and_settle(&mut rig, &file, &format!("export T={SECRET}\n"));
    assert_eq!(rig.engine.db.inbox_count().unwrap(), 1);
    // The user edits the secret out; the clean commit makes the held
    // change moot — the item must not sit there inviting a stale
    // approval.
    edit_and_settle(&mut rig, &file, "export A=2\n");
    assert_eq!(rig.engine.db.inbox_count().unwrap(), 0);
    assert_eq!(store_content(&rig, &file).as_deref(), Some("export A=2\n"));
}

#[test]
fn crash_loop_is_noticed_from_the_event_trail() {
    let mut rig = rig();
    // Five extra starts (Engine::new logged one) — under the bar.
    for _ in 0..4 {
        soft(
            rig.engine
                .db
                .record(EventKind::DaemonStarted, "testbox", ""),
        );
    }
    assert_eq!(rig.engine.health_tick_forced(), 0);
    soft(
        rig.engine
            .db
            .record(EventKind::DaemonStarted, "testbox", ""),
    );
    let new = rig.engine.health_tick_forced();
    assert_eq!(new, 1);
    let item = &rig.engine.db.inbox_open().unwrap()[0];
    assert_eq!(item.subject, "daemon");
    assert!(
        item.body.contains("daemon starts in the last hour"),
        "{}",
        item.body
    );
}
