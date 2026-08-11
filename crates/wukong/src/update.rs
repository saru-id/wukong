//! `wukong update`: fetch the latest release, verify it, swap the
//! binaries this very command is running from, restart the daemon.
//! Manual on purpose — the daemon never phones home; updating is a
//! decision, not a background surprise.

use std::path::Path;

const REPO: &str = "saru-id/wukong";

pub fn run(check_only: bool, rollback: bool) -> anyhow::Result<()> {
    if rollback {
        return roll_back();
    }
    let current = env!("CARGO_PKG_VERSION");
    let api = curl(&format!(
        "https://api.github.com/repos/{REPO}/releases/latest"
    ))?;
    let release: serde_json::Value = serde_json::from_slice(&api)?;
    let tag = release["tag_name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("release feed has no tag"))?
        .to_string();
    if tag == format!("v{current}") {
        println!("already the newest release ({tag})");
        return Ok(());
    }
    println!("v{current} → {tag}");
    if let Some(notes) = release["name"].as_str() {
        println!("  {notes}");
    }
    if check_only {
        println!("run `wukong update` to install it");
        return Ok(());
    }

    let name = format!("wukong-{tag}-aarch64-apple-darwin");
    let base = format!("https://github.com/{REPO}/releases/download/{tag}");
    let work = tempfile::TempDir::new()?;
    let tarball = work.path().join(format!("{name}.tar.gz"));
    std::fs::write(&tarball, curl(&format!("{base}/{name}.tar.gz"))?)?;

    // The published checksum is the contract: refuse anything else.
    let sha_line = String::from_utf8_lossy(&curl(&format!("{base}/{name}.tar.gz.sha256"))?)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase();
    let actual = sha256_hex(&std::fs::read(&tarball)?);
    anyhow::ensure!(
        !sha_line.is_empty() && sha_line == actual,
        "checksum verification FAILED — refusing to update"
    );
    println!("✓ downloaded and verified");

    let status = std::process::Command::new("tar")
        .arg("xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(work.path())
        .status()?;
    anyhow::ensure!(status.success(), "could not unpack the release");
    let stage = work.path().join(&name);

    // Swap the binaries where the running ones live: write beside,
    // then rename over — the running processes keep their old inodes.
    let bin_dir = std::env::current_exe()?
        .parent()
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("cannot locate the install directory"))?;
    for binary in ["wukong", "wukongd"] {
        let target = bin_dir.join(binary);
        // Keep the outgoing pair: an upgrade that goes wrong is a
        // ten-second `wukong update --rollback`, not an incident.
        if target.is_file() {
            let _ = std::fs::copy(&target, target.with_extension("prev"));
        }
        replace(&stage.join(binary), &target)?;
    }
    println!(
        "✓ binaries updated in {} (previous kept as .prev)",
        bin_dir.display()
    );

    // A running agent restarts onto the new daemon; without one, the
    // next start picks it up.
    if crate::launchd::agent_path().exists() {
        let ok = crate::launchd::kickstart();
        println!(
            "{}",
            if ok {
                "✓ daemon restarted on the new version"
            } else {
                "note: restart the daemon yourself: wukong daemon restart"
            }
        );
    }
    // The proof: does the daemon actually answer as the new version?
    match wait_for_daemon_version() {
        Some(version) => println!("✓ daemon answering as v{version}"),
        None => println!(
            "⚠ daemon not answering on the new version — read \
             ~/.local/state/wukong/wukongd.log, and `wukong update --rollback` \
             restores the previous binaries"
        ),
    }
    println!("updated to {tag}");
    Ok(())
}

/// Swap the `.prev` pair back and restart. If the newer daemon
/// already migrated the database, the notice explains the way out —
/// the database is rebuildable from the store.
fn roll_back() -> anyhow::Result<()> {
    let bin_dir = std::env::current_exe()?
        .parent()
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("cannot locate the install directory"))?;
    for binary in ["wukong", "wukongd"] {
        let prev = bin_dir.join(binary).with_extension("prev");
        anyhow::ensure!(
            prev.is_file(),
            "no previous binaries kept here — nothing to roll back to"
        );
        replace(&prev, &bin_dir.join(binary))?;
    }
    println!("✓ previous binaries restored");
    if crate::launchd::agent_path().exists() {
        crate::launchd::kickstart();
    }
    match wait_for_daemon_version() {
        Some(version) => println!("✓ daemon answering as v{version}"),
        None => println!(
            "⚠ daemon not answering — if the newer version migrated the \
             database, the older daemon refuses it. The database is \
             rebuildable: delete ~/.local/share/wukong/wukong.db and the \
             roster self-heals from the store (allowances re-quarantine)."
        ),
    }
    Ok(())
}

/// Up to ten seconds for the (re)started daemon to answer, reporting
/// the version it answers WITH.
fn wait_for_daemon_version() -> Option<String> {
    use wukong_core::ipc::{Request, Response};
    for _ in 0..50 {
        if let Ok(Response::Pong { daemon_version, .. }) = crate::client::call(Request::Ping) {
            return Some(daemon_version);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    None
}

fn replace(new: &Path, target: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let staged = target.with_extension("new");
    std::fs::copy(new, &staged)?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&staged, target)?;
    Ok(())
}

/// macOS ships curl; wukong keeps its no-HTTP-stack posture (same
/// reasoning as shelling to git).
fn curl(url: &str) -> anyhow::Result<Vec<u8>> {
    let out = std::process::Command::new("curl")
        .args(["-fsSL", "--max-time", "120", url])
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "download failed: {url}\n{}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(out.stdout)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(bytes);
    digest.iter().fold(String::new(), |mut hex, b| {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
        hex
    })
}
