//! Timestamped lines for the daemon log. launchd redirects stderr to
//! wukongd.log; a line without a time is half a log entry.

/// One log line, RFC3339-stamped to the second.
pub fn emit(msg: impl std::fmt::Display) {
    let ts = jiff::Timestamp::now()
        .round(jiff::Unit::Second)
        .unwrap_or_else(|_| jiff::Timestamp::now());
    eprintln!("{ts} {msg}");
}
