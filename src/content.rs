//! Small shared helpers for handling file contents.

/// How much of a file is inspected when guessing whether it is binary.
pub const BINARY_SNIFF_BYTES: usize = 8192;

/// Cap on tool output returned inline, per stream.
///
/// Anything beyond this is preserved via [`crate::spill`] rather than dropped.
pub const MAX_OUTPUT_BYTES: usize = 256 * 1024;

/// Whether the bytes look like a binary file.
///
/// A NUL in the first few kilobytes is the standard heuristic, and the same one
/// `grep` uses. Counting words or running a regex over a binary produces noise,
/// so tools skip these rather than mangling them.
pub fn looks_binary(bytes: &[u8]) -> bool {
    memchr::memchr(0, &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)]).is_some()
}

/// Writes a file by creating a temporary alongside it and renaming into place.
///
/// A crash midway through a plain write leaves the file truncated; the rename
/// is atomic, so a reader sees either the old contents or the new. The original
/// permissions are preserved, which matters for scripts.
pub fn write_atomically(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    let directory = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(directory)?;
    std::io::Write::write_all(&mut temp, contents)?;

    // NamedTempFile creates 0600; restore whatever the original had.
    #[cfg(unix)]
    if let Ok(metadata) = std::fs::metadata(path) {
        use std::os::unix::fs::PermissionsExt;
        let _ = temp
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(metadata.permissions().mode()));
    }

    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_not_mistaken_for_binary() {
        assert!(!looks_binary(b"hello world\n"));
        assert!(!looks_binary("h\u{e9}llo".as_bytes()));
        assert!(!looks_binary(b""));
    }

    #[test]
    fn a_nul_byte_marks_content_as_binary() {
        assert!(looks_binary(b"\x00\x01"));
        assert!(looks_binary(b"text then\x00more"));
    }

    #[test]
    fn only_the_first_few_kilobytes_are_inspected() {
        // A NUL past the sniff window is not detected, matching grep. This is a
        // deliberate trade: reading the whole file to classify it would defeat
        // the point of the check.
        let mut data = vec![b'a'; BINARY_SNIFF_BYTES + 16];
        data.push(0);
        assert!(!looks_binary(&data));
    }

    #[test]
    fn an_atomic_write_replaces_the_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, b"old").unwrap();

        write_atomically(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn an_atomic_write_creates_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh.txt");
        write_atomically(&path, b"hello").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
    }

    #[cfg(unix)]
    #[test]
    fn permissions_survive_an_atomic_write() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script.sh");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        write_atomically(&path, b"new").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "the executable bit must not be dropped");
    }

    #[test]
    fn no_temporary_file_is_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        write_atomically(&path, b"x").unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
