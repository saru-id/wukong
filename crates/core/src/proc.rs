//! Child processes, always on a leash.
//!
//! wukong asks other programs things: brew what a name means, npm where
//! its global root is, `security` for the seal identity. Every one of
//! them can wedge — a broken shim, a node runtime waiting on a lock, a
//! securityd that stopped answering — and a governor that waits forever
//! on any of them has stopped governing. So there is one way to run a
//! child here, and it has a wall clock. A wedged child costs its caller
//! one feature, never the process it runs in.
//!
//! Output is piped and read after exit, which suits tools that answer
//! in kilobytes. Anything that streams belongs somewhere else.

use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Run a program to completion under `secs`, or kill it and answer
/// `None`. `None` also covers a program that could not be started at
/// all — from the caller's side those are the same answer: this tool
/// did not tell us anything.
#[must_use]
pub fn bounded(binary: &str, args: &[&str], secs: u64) -> Option<Output> {
    let mut child = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
}

/// The stdout of a successful run, or `None` for anything else.
#[must_use]
pub fn bounded_output(binary: &str, args: &[&str], secs: u64) -> Option<String> {
    let out = bounded(binary, args, secs)?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_that_never_finishes_costs_its_deadline_and_nothing_more() {
        let start = Instant::now();
        assert!(bounded("sleep", &["120"], 1).is_none());
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the deadline did not fire: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn a_program_that_is_not_there_is_just_no_answer() {
        assert!(bounded("wukong-no-such-binary", &[], 5).is_none());
        assert!(bounded_output("wukong-no-such-binary", &[], 5).is_none());
    }

    #[test]
    fn failure_keeps_its_exit_code_while_output_keeps_only_success() {
        let out = bounded("false", &[], 5).expect("ran");
        assert_eq!(out.status.code(), Some(1));
        assert!(bounded_output("false", &[], 5).is_none());
        assert_eq!(
            bounded_output("echo", &["hello"], 5).as_deref(),
            Some("hello\n")
        );
    }
}
