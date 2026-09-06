//! Confining tools to a set of directories.
//!
//! Every server that touches the filesystem needs the same thing: resolve a
//! caller-supplied path, refuse anything outside the configured roots, and
//! refuse files too large to hold in memory. Each had its own copy, and they
//! had already drifted — only one enforced the size cap.

use std::path::{Path, PathBuf};

use crate::paths;
use crate::tool::{ToolFailure, ToolResult};

/// Default ceiling on a single file read.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// A set of permitted roots and a size ceiling.
#[derive(Debug, Clone)]
pub struct Sandbox {
    roots: Vec<PathBuf>,
    max_file_bytes: u64,
}

impl Default for Sandbox {
    fn default() -> Self {
        Self { roots: Vec::new(), max_file_bytes: DEFAULT_MAX_FILE_BYTES }
    }
}

impl Sandbox {
    /// An empty root set means unrestricted, which is the right default for a
    /// server the operator runs against their own machine.
    pub fn new(roots: &[PathBuf], max_file_bytes: u64) -> Self {
        // Resolve once, so a symlinked root (`/tmp` on macOS, `/home` on many
        // Linux installs) still matches paths resolved through it.
        let roots = roots.iter().map(|root| paths::resolve_for_comparison(root)).collect();
        Self { roots, max_file_bytes }
    }

    pub fn unrestricted(max_file_bytes: u64) -> Self {
        Self { roots: Vec::new(), max_file_bytes }
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub fn max_file_bytes(&self) -> u64 {
        self.max_file_bytes
    }

    /// Resolves a caller-supplied path, refusing anything outside the roots.
    pub fn resolve(&self, raw: &str) -> ToolResult<PathBuf> {
        if raw.trim().is_empty() {
            return Err(ToolFailure::InvalidArguments("path must not be empty".into()));
        }
        let requested = Path::new(raw);
        if requested.is_relative() {
            return Err(ToolFailure::InvalidArguments(format!(
                "path {raw:?} must be absolute; the server has no meaningful working directory"
            )));
        }

        // Canonicalise when it exists so symlinks cannot step outside a root;
        // normalise lexically when it does not, so creating a file still works.
        let resolved = paths::resolve_for_comparison(requested);

        if self.roots.is_empty() || self.roots.iter().any(|root| paths::is_within(&resolved, root))
        {
            return Ok(resolved);
        }
        Err(ToolFailure::Denied(format!(
            "path {raw:?} is outside the permitted directories ({})",
            self.describe_roots()
        )))
    }

    /// Refuses a file larger than the ceiling, returning its size otherwise.
    ///
    /// Several tools read a whole file into memory to rewrite it, so without
    /// this a large target exhausts RAM.
    pub fn check_size(&self, path: &Path) -> ToolResult<u64> {
        let length = std::fs::metadata(path)
            .map_err(|e| ToolFailure::Failed(format!("could not stat {}: {e}", path.display())))?
            .len();
        if length > self.max_file_bytes {
            return Err(ToolFailure::Denied(format!(
                "{} is {length} bytes, over the {} byte limit; raise the size limit to proceed",
                path.display(),
                self.max_file_bytes
            )));
        }
        Ok(length)
    }

    fn describe_roots(&self) -> String {
        if self.roots.is_empty() {
            return "none configured".to_string();
        }
        self.roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        (dir, root)
    }

    #[test]
    fn an_empty_root_set_permits_any_absolute_path() {
        let sandbox = Sandbox::unrestricted(1 << 20);
        assert!(sandbox.resolve("/etc/hostname").is_ok());
    }

    #[test]
    fn relative_and_empty_paths_are_refused() {
        let sandbox = Sandbox::unrestricted(1 << 20);
        assert!(sandbox.resolve("relative/file").is_err());
        assert!(sandbox.resolve("   ").is_err());
    }

    #[test]
    fn paths_inside_a_root_resolve_and_others_are_denied() {
        let (_dir, root) = temp_root();
        std::fs::write(root.join("f.txt"), b"x").unwrap();

        let sandbox = Sandbox::new(std::slice::from_ref(&root), 1 << 20);
        assert!(sandbox.resolve(root.join("f.txt").to_str().unwrap()).is_ok());
        assert!(matches!(sandbox.resolve("/etc/passwd"), Err(ToolFailure::Denied(_))));
    }

    #[test]
    fn the_denial_names_the_permitted_directories() {
        let (_dir, root) = temp_root();
        let sandbox = Sandbox::new(std::slice::from_ref(&root), 1 << 20);
        let err = sandbox.resolve("/etc/passwd").unwrap_err().to_string();
        assert!(err.contains(&root.display().to_string()), "{err}");
    }

    #[test]
    fn dot_dot_cannot_escape_a_root() {
        let (_dir, root) = temp_root();
        let sandbox = Sandbox::new(std::slice::from_ref(&root), 1 << 20);
        let escape = format!("{}/../../../../etc/passwd", root.display());
        assert!(matches!(sandbox.resolve(&escape), Err(ToolFailure::Denied(_))));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_leaving_the_root_is_denied() {
        let (_dir, root) = temp_root();
        let link = root.join("escape");
        std::os::unix::fs::symlink("/etc/passwd", &link).unwrap();

        let sandbox = Sandbox::new(std::slice::from_ref(&root), 1 << 20);
        assert!(matches!(sandbox.resolve(link.to_str().unwrap()), Err(ToolFailure::Denied(_))));
    }

    #[test]
    fn a_file_that_does_not_exist_yet_may_still_be_created_inside_a_root() {
        let (_dir, root) = temp_root();
        let sandbox = Sandbox::new(std::slice::from_ref(&root), 1 << 20);
        assert!(sandbox.resolve(root.join("new.txt").to_str().unwrap()).is_ok());
    }

    #[test]
    fn a_sibling_directory_sharing_a_prefix_is_not_inside_the_root() {
        let (dir, root) = temp_root();
        let sibling = dir.path().parent().map(|p| p.join("unrelated"));
        let _ = sibling;

        // Constructed directly: `/data-private` must not pass as `/data`.
        let sandbox = Sandbox::new(&[PathBuf::from("/data")], 1 << 20);
        assert!(matches!(sandbox.resolve("/data-private/x"), Err(ToolFailure::Denied(_))));
        drop(root);
    }

    #[test]
    fn the_size_ceiling_is_enforced() {
        let (_dir, root) = temp_root();
        let path = root.join("big");
        std::fs::write(&path, vec![0u8; 4096]).unwrap();

        let tight = Sandbox::new(std::slice::from_ref(&root), 1024);
        assert!(matches!(tight.check_size(&path), Err(ToolFailure::Denied(_))));

        let roomy = Sandbox::new(std::slice::from_ref(&root), 1 << 20);
        assert_eq!(roomy.check_size(&path).unwrap(), 4096);
    }

    #[test]
    fn sizing_a_missing_file_reports_the_stat_failure() {
        assert!(Sandbox::unrestricted(1 << 20).check_size(Path::new("/nope/x")).is_err());
    }
}
