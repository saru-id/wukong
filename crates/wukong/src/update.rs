//! `wukong update`: fetch the latest release, verify it, swap the
//! binaries this very command is running from, restart the daemon.
//! Manual on purpose — the daemon never phones home; updating is a
//! decision, not a background surprise.

use std::path::Path;

const REPO: &str = "saru-id/wukong";

pub fn run(check_only: bool) -> anyhow::Result<()> {
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
        replace(&stage.join(binary), &bin_dir.join(binary))?;
    }
    println!("✓ binaries updated in {}", bin_dir.display());

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
    println!("updated to {tag}");
    Ok(())
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
