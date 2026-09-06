//! Preserving output too large to return inline.
//!
//! Capping a tool's output protects the caller's context window and the
//! server's memory, but discarding the excess is hostile: the only way to see
//! the rest is to run the command again, and the command may be expensive,
//! non-deterministic, or destructive. Re-running `rm -rf` to read its output is
//! not a reasonable thing to ask.
//!
//! So the excess is written to a file and the caller is told where. Memory
//! stays bounded because only the head is retained in RAM — everything else
//! streams straight to disk as it arrives.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Where captured output is kept, and how much of it.
#[derive(Debug, Clone)]
pub struct SpillDir {
    root: PathBuf,
    /// How many spill files to keep. Older ones are removed, so a long-running
    /// server does not slowly fill the disk with forgotten output.
    retain: usize,
}

/// Output that did not fit inline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spilled {
    pub path: PathBuf,
    pub total_bytes: u64,
}

impl SpillDir {
    pub fn new(root: impl Into<PathBuf>, retain: usize) -> Self {
        Self { root: root.into(), retain: retain.max(1) }
    }

    /// The default location: a per-server directory under the system temp dir.
    pub fn for_server(server: &str) -> Self {
        Self::new(std::env::temp_dir().join(format!("{server}-mcp-output")), 32)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Opens a sink for one stream. The file is created lazily, so output that
    /// fits inline never touches the disk.
    pub fn sink(&self, label: &str, head_limit: usize) -> Sink {
        Sink {
            dir: self.clone(),
            label: sanitize(label),
            head_limit,
            head: Vec::with_capacity(head_limit.min(64 * 1024)),
            total: 0,
            file: None,
            path: None,
            written: 0,
            failed: None,
        }
    }

    /// Removes all but the newest `retain` files.
    fn prune(&self) {
        let Ok(entries) = std::fs::read_dir(&self.root) else { return };
        let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
            .flatten()
            .filter_map(|e| {
                let modified = e.metadata().ok()?.modified().ok()?;
                Some((modified, e.path()))
            })
            .collect();

        if files.len() <= self.retain {
            return;
        }
        files.sort_by_key(|(time, _)| *time);
        for (_, path) in &files[..files.len() - self.retain] {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Accumulates one output stream, keeping the head in memory and the whole
/// thing on disk once it grows past the limit.
pub struct Sink {
    dir: SpillDir,
    label: String,
    head_limit: usize,
    head: Vec<u8>,
    total: u64,
    file: Option<File>,
    path: Option<PathBuf>,
    /// Bytes of the stream already on disk, so the chunk that triggers the
    /// spill does not get written twice — once via the buffered head and once
    /// as itself.
    written: u64,
    /// Why spilling failed, if it did. A failed spill must not fail the tool
    /// call — partial output beats none.
    failed: Option<String>,
}

impl Sink {
    /// Adds a chunk. Never fails the caller: a disk problem degrades to
    /// ordinary truncation.
    pub fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len() as u64;

        let room = self.head_limit.saturating_sub(self.head.len());
        if room > 0 {
            self.head.extend_from_slice(&chunk[..room.min(chunk.len())]);
        }

        // Everything is written once we know the output overflows, including
        // the head we already buffered, so the file holds the complete stream.
        if self.total > self.head_limit as u64 {
            if self.file.is_none() && self.failed.is_none() {
                self.open();
            }
            if let Some(file) = self.file.as_mut() {
                let outstanding = usize::try_from(self.total - self.written).unwrap_or(usize::MAX);
                let start = chunk.len().saturating_sub(outstanding);
                if let Err(err) = file.write_all(&chunk[start..]) {
                    self.failed = Some(err.to_string());
                    self.file = None;
                } else {
                    self.written = self.total;
                }
            }
        }
    }

    fn open(&mut self) {
        if let Err(err) = std::fs::create_dir_all(&self.dir.root) {
            self.failed = Some(err.to_string());
            return;
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = self.dir.root.join(format!("{}-{stamp}.txt", self.label));

        match create_private(&path) {
            Ok(mut file) => {
                // The head was buffered before we knew we would spill; write it
                // first so the file is the complete stream, not just the tail.
                if let Err(err) = file.write_all(&self.head) {
                    self.failed = Some(err.to_string());
                    return;
                }
                self.written = self.head.len() as u64;
                self.file = Some(file);
                self.path = Some(path);
            }
            Err(err) => self.failed = Some(err.to_string()),
        }
    }

    /// Finishes the stream, returning what to show inline and where the rest is.
    pub fn finish(mut self) -> Captured {
        if let Some(file) = self.file.as_mut() {
            let _ = file.flush();
        }
        let spilled = self.path.clone().map(|path| Spilled { path, total_bytes: self.total });
        if spilled.is_some() {
            self.dir.prune();
        }

        Captured {
            head: String::from_utf8_lossy(&self.head).into_owned(),
            total_bytes: self.total,
            truncated: self.total > self.head_limit as u64,
            spilled,
            spill_error: self.failed,
        }
    }
}

/// The result of capturing one stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    /// The first `head_limit` bytes, safe to return inline.
    pub head: String,
    pub total_bytes: u64,
    pub truncated: bool,
    /// Where the complete output was written, when it did not fit.
    pub spilled: Option<Spilled>,
    /// Set when spilling was attempted and failed.
    pub spill_error: Option<String>,
}

impl Captured {
    /// A note telling the caller how to reach the rest, or `None` when nothing
    /// was lost.
    pub fn notice(&self) -> Option<String> {
        if !self.truncated {
            return None;
        }
        Some(match (&self.spilled, &self.spill_error) {
            (Some(spill), _) => format!(
                "[showing the first {} of {} bytes. The complete output is at {} — read or \
                 search that file rather than running this again.]",
                self.head.len(),
                spill.total_bytes,
                spill.path.display()
            ),
            (None, Some(err)) => format!(
                "[output truncated at {} of {} bytes; the rest could not be saved ({err})]",
                self.head.len(),
                self.total_bytes
            ),
            (None, None) => {
                format!("[output truncated at {} bytes]", self.head.len())
            }
        })
    }

    /// The inline text with the notice appended.
    pub fn text_with_notice(&self) -> String {
        match self.notice() {
            Some(notice) => format!("{}\n{notice}", self.head),
            None => self.head.clone(),
        }
    }
}

/// Creates a file readable only by its owner: captured output may contain
/// anything the command printed, including credentials.
fn create_private(path: &Path) -> std::io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Keeps a caller-supplied label from escaping the spill directory.
fn sanitize(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    if cleaned.is_empty() { "output".to_string() } else { cleaned }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> (tempfile::TempDir, SpillDir) {
        let temp = tempfile::tempdir().unwrap();
        let spill = SpillDir::new(temp.path().join("spill"), 4);
        (temp, spill)
    }

    #[test]
    fn small_output_stays_inline_and_never_touches_disk() {
        let (_temp, spill) = dir();
        let mut sink = spill.sink("stdout", 1024);
        sink.push(b"hello");
        let captured = sink.finish();

        assert_eq!(captured.head, "hello");
        assert!(!captured.truncated);
        assert!(captured.spilled.is_none());
        assert!(captured.notice().is_none());
        assert!(!spill.root().exists(), "nothing should have been written");
    }

    #[test]
    fn large_output_is_preserved_in_full_on_disk() {
        let (_temp, spill) = dir();
        let mut sink = spill.sink("stdout", 10);
        sink.push(b"0123456789ABCDEFGHIJ");
        let captured = sink.finish();

        assert_eq!(captured.head, "0123456789", "only the head is returned inline");
        assert!(captured.truncated);
        assert_eq!(captured.total_bytes, 20);

        let spilled = captured.spilled.expect("large output must be preserved");
        let contents = std::fs::read_to_string(&spilled.path).unwrap();
        // The whole stream, not just the part past the cap.
        assert_eq!(contents, "0123456789ABCDEFGHIJ");
        assert_eq!(spilled.total_bytes, 20);
    }

    #[test]
    fn the_notice_points_at_the_file_and_discourages_re_running() {
        let (_temp, spill) = dir();
        let mut sink = spill.sink("stdout", 4);
        sink.push(b"abcdefgh");
        let captured = sink.finish();

        let notice = captured.notice().unwrap();
        assert!(notice.contains("complete output is at"), "{notice}");
        assert!(notice.contains("rather than running this again"), "{notice}");
        assert!(captured.text_with_notice().starts_with("abcd"));
    }

    #[test]
    fn output_arriving_in_many_chunks_is_reassembled() {
        let (_temp, spill) = dir();
        let mut sink = spill.sink("stdout", 5);
        for chunk in ["aaa", "bbb", "ccc", "ddd"] {
            sink.push(chunk.as_bytes());
        }
        let captured = sink.finish();

        assert_eq!(captured.head, "aaabb");
        let spilled = captured.spilled.unwrap();
        assert_eq!(std::fs::read_to_string(spilled.path).unwrap(), "aaabbbcccddd");
    }

    #[test]
    fn output_exactly_at_the_limit_is_not_treated_as_truncated() {
        let (_temp, spill) = dir();
        let mut sink = spill.sink("stdout", 5);
        sink.push(b"12345");
        let captured = sink.finish();

        assert!(!captured.truncated);
        assert!(captured.spilled.is_none());
    }

    #[test]
    fn old_spill_files_are_pruned() {
        let (_temp, spill) = dir(); // retains 4
        for _ in 0..7 {
            let mut sink = spill.sink("stdout", 2);
            sink.push(b"aaaaaaaa");
            sink.finish();
            // Distinct mtimes so ordering is well defined.
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let count = std::fs::read_dir(spill.root()).unwrap().count();
        assert_eq!(count, 4, "a long-running server must not fill the disk");
    }

    #[test]
    fn a_failed_spill_degrades_to_truncation_rather_than_failing_the_call() {
        // Point the directory at a path that cannot be created.
        let spill = SpillDir::new("/proc/version/cannot-create-here", 4);
        let mut sink = spill.sink("stdout", 4);
        sink.push(b"abcdefgh");
        let captured = sink.finish();

        assert_eq!(captured.head, "abcd", "partial output beats none");
        assert!(captured.spilled.is_none());
        assert!(captured.spill_error.is_some());
        assert!(captured.notice().unwrap().contains("could not be saved"));
    }

    #[test]
    fn a_hostile_label_cannot_escape_the_directory() {
        assert_eq!(sanitize("../../etc/passwd"), "------etc-passwd");
        assert_eq!(sanitize("std/out"), "std-out");
        assert_eq!(sanitize(""), "output");
    }

    #[cfg(unix)]
    #[test]
    fn spill_files_are_private_to_their_owner() {
        use std::os::unix::fs::PermissionsExt;

        let (_temp, spill) = dir();
        let mut sink = spill.sink("stdout", 2);
        sink.push(b"secret token in output");
        let spilled = sink.finish().spilled.unwrap();

        let mode = std::fs::metadata(&spilled.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "captured output may contain credentials");
    }

    #[test]
    fn empty_output_produces_nothing() {
        let (_temp, spill) = dir();
        let captured = spill.sink("stdout", 16).finish();
        assert!(captured.head.is_empty());
        assert!(!captured.truncated);
        assert!(captured.spilled.is_none());
    }
}
