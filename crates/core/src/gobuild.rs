//! Reading the build metadata Go embeds in every binary: the module
//! path `go install` would rebuild it from, and the version it was
//! built at. Pure byte scanning of the binary on disk — no toolchain
//! fork, no Mach-O parsing (the magic is unique enough to find
//! directly). Binaries without the blob (not Go, or stripped by
//! unusual builds) simply read as `None` and are left ungoverned.

/// The buildinfo header magic (`\xff` guarantees it can't appear in
/// legitimate strings).
const MAGIC: &[u8] = b"\xff Go buildinf:";

/// Module info is wrapped in 16-byte sentinels by the Go linker.
const SENTINEL: usize = 16;

/// Extract (module path, version) from a Go binary's bytes. The path
/// is the `go install`-able identity ("github.com/junegunn/fzf"); the
/// version is best-effort (locally built binaries carry "(devel)",
/// which reads as no version).
#[must_use]
pub fn read(bytes: &[u8]) -> Option<(String, Option<String>)> {
    let at = find(bytes, MAGIC)?;
    let header = bytes.get(at + MAGIC.len()..)?;
    // header[0] is the pointer size, header[1] the flags. Bit 1 set
    // means the strings follow inline as varint-prefixed UTF-8 —
    // always the case for the module era of Go; anything older has no
    // module path to govern anyway.
    if header.get(1)? & 0b10 == 0 {
        return None;
    }
    let (_go_version, rest) = varint_string(header.get(2..)?)?;
    let (modinfo, _) = varint_string(rest)?;
    let inner = modinfo.get(SENTINEL..modinfo.len().checked_sub(SENTINEL)?)?;
    let text = std::str::from_utf8(inner).ok()?;
    let mut path = None;
    let mut version = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("path\t") {
            path = Some(p.to_string());
        } else if let Some(m) = line.strip_prefix("mod\t") {
            version = m.split('\t').nth(1).map(str::to_string);
        }
    }
    Some((path?, version.filter(|v| v != "(devel)")))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Unsigned LEB128, as encoded by Go's binary.PutUvarint.
fn uvarint(bytes: &[u8]) -> Option<(u64, &[u8])> {
    let mut value = 0u64;
    let mut shift = 0u32;
    for (i, &b) in bytes.iter().enumerate() {
        if i == 10 {
            return None;
        }
        if b < 0x80 {
            return Some((value | (u64::from(b) << shift), &bytes[i + 1..]));
        }
        value |= u64::from(b & 0x7f) << shift;
        shift += 7;
    }
    None
}

fn varint_string(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let (len, rest) = uvarint(bytes)?;
    let len = usize::try_from(len).ok()?;
    (rest.len() >= len).then(|| rest.split_at(len))
}

/// Build a synthetic buildinfo blob — the test/drill stand-in for a
/// real Go binary.
#[must_use]
pub fn synthesize(module_path: &str, version: &str) -> Vec<u8> {
    fn put_string(out: &mut Vec<u8>, s: &[u8]) {
        let mut len = s.len();
        while len >= 0x80 {
            out.push(u8::try_from(len & 0x7f).expect("masked") | 0x80);
            len >>= 7;
        }
        out.push(u8::try_from(len).expect("below 0x80"));
        out.extend_from_slice(s);
    }
    let mut out = b"not-go-bytes-before-the-magic".to_vec();
    out.extend_from_slice(MAGIC);
    out.push(8); // pointer size
    out.push(0b10); // inline strings
    put_string(&mut out, b"go1.24.0");
    let sentinel = [0xabu8; SENTINEL];
    let mut modinfo = sentinel.to_vec();
    modinfo.extend_from_slice(
        format!("path\t{module_path}\nmod\t{module_path}\t{version}\th1:x=\n").as_bytes(),
    );
    modinfo.extend_from_slice(&sentinel);
    put_string(&mut out, &modinfo);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_a_synthetic_binary() {
        let bytes = synthesize("github.com/junegunn/fzf", "v0.46.1");
        assert_eq!(
            read(&bytes),
            Some((
                "github.com/junegunn/fzf".to_string(),
                Some("v0.46.1".to_string())
            ))
        );
    }

    #[test]
    fn devel_builds_read_as_versionless() {
        let bytes = synthesize("example.com/tool", "(devel)");
        assert_eq!(read(&bytes), Some(("example.com/tool".to_string(), None)));
    }

    #[test]
    fn non_go_bytes_and_truncation_are_safe() {
        assert_eq!(read(b"just an ordinary binary"), None);
        let bytes = synthesize("example.com/tool", "v1.0.0");
        for cut in [10, MAGIC.len() + 30, bytes.len() - 20] {
            assert_eq!(read(&bytes[..cut]), None, "cut at {cut}");
        }
    }
}
