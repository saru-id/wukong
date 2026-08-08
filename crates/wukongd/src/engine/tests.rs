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
    let config = Config {
        machine: "testbox".to_string(),
        debounce_secs: 0,
        ..Config::default()
    };
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
    let resp = rig.engine.track(file.to_str().unwrap());
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
    let resp = rig.engine.track(env.to_str().unwrap());
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

    rig.engine.resolve(ht.id, Resolution::Ignore);
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
    rig.engine.pkg_record(Provider::Formula, "jq", false, false);
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
    rig.engine.pkg_record(Provider::Formula, "fd", false, false);
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
    rig.engine.pkg_record(Provider::Formula, "jq", false, false);
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
