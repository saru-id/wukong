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
    // process per notification until the daemon exits. The thread is
    // off the event loop, but "off the event loop" is not "free": an
    // osascript waiting on a permission prompt nobody answers would
    // hold its thread and its child for the daemon's whole life, one
    // pair per new inbox item. A banner is worth seconds, not a leak.
    std::thread::spawn(move || {
        // Best effort by design: a banner that did not appear is not
        // something the governor can or should do anything about.
        let _ = wukong_core::proc::bounded("osascript", &["-e", &script], NOTIFY_TIMEOUT_SECS);
    });
}

/// Long enough for a busy Notification Center, short enough that a
/// prompt nobody answers cannot accumulate.
const NOTIFY_TIMEOUT_SECS: u64 = 20;

fn applescript_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}
