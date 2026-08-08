//! macOS notifications, fired only on new inbox items so the daemon
//! stays quiet. Best-effort: osascript is always present on macOS, and
//! a failure to notify never disturbs the governor.

pub fn inbox(new_items: usize) {
    let noun = if new_items == 1 { "item" } else { "items" };
    let body = format!("{new_items} new {noun} in the wukong inbox");
    let script = format!(
        "display notification {} with title \"wukong\"",
        applescript_quote(&body)
    );
    // A reaping thread, not a bare spawn — bare spawn leaks one zombie
    // process per notification until the daemon exits.
    std::thread::spawn(move || {
        let _ = std::process::Command::new("osascript")
            .args(["-e", &script])
            .status();
    });
}

fn applescript_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}
