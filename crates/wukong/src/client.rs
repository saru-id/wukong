//! The client half of the IPC contract: connect to the daemon socket,
//! send one request, read one response. Synchronous and tiny — the CLI
//! and TUI both borrow it.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use wukong_core::ipc::{Envelope, Request, Response};
use wukong_core::paths;

pub fn connected() -> bool {
    UnixStream::connect(paths::socket_file()).is_ok()
}

/// One round trip. Returns a friendly error when the daemon is down.
pub fn call(req: Request) -> anyhow::Result<Response> {
    let mut stream = UnixStream::connect(paths::socket_file()).map_err(|_| {
        anyhow::anyhow!("wukongd is not running — start it with `wukong daemon start`")
    })?;
    let line = serde_json::to_string(&Envelope::new(req))?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response)?;
    Ok(serde_json::from_str(&response)?)
}
