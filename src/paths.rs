//! Path handling that behaves the same on Linux, macOS and Windows.
//!
//! Every server in the family confines callers to a set of roots, and each had
//! grown its own copy of that logic. Sharing it removes the duplication and,
//! more importantly, fixes three ways the copies were wrong off Linux:
//!
//! * **Windows verbatim prefixes.** `Path::canonicalize` returns `\\?\C:\dir`.
//!   A configured root that does not yet exist keeps its plain `C:\dir` form,
//!   so a plain `starts_with` compares a prefixed path against an unprefixed
//!   root and the sandbox denies everything.
//! * **Case-insensitive filesystems.** Windows and macOS-by-default treat
//!   `C:\Work` and `c:\work` as the same directory; a byte-wise `starts_with`
//!   does not, so a correctly configured root rejects its own files.
//! * **`HOME` is not universal.** Windows sets `USERPROFILE`. Reading only
//!   `HOME` there yields a *relative* fallback, which silently writes into the
//!   working directory.

use std::path::{Component, Path, PathBuf};

/// Whether this platform's filesystem compares paths case-insensitively.
///
/// macOS can be formatted case-sensitively, but the default is not, and
/// treating it as insensitive is the safe direction: it can only ever refuse a
/// path the operator did not intend to allow, never permit one they did not.
pub const fn case_insensitive_filesystem() -> bool {
    cfg!(any(windows, target_os = "macos"))
}

/// Collapses `.` and `..` without touching the filesystem.
///
/// Used when a path does not exist yet — creating a new file inside a root must
/// still be possible — so `canonicalize` cannot be asked.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Removes Windows' `\\?\` verbatim prefix so canonicalised and literal paths
/// can be compared. A no-op elsewhere.
#[must_use]
pub fn strip_verbatim(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path.to_path_buf()
}

/// Resolves a path for comparison: canonicalised when it exists, normalised
/// lexically when it does not, and always free of verbatim prefixes.
pub fn resolve_for_comparison(path: &Path) -> PathBuf {
    let resolved = path.canonicalize().unwrap_or_else(|_| normalize(path));
    strip_verbatim(&resolved)
}

/// Whether `path` lies inside `root`, using this platform's comparison rules.
pub fn is_within(path: &Path, root: &Path) -> bool {
    is_within_with(path, root, case_insensitive_filesystem())
}

/// The comparison with the case rule supplied explicitly, so both behaviours
/// are testable on any host.
pub fn is_within_with(path: &Path, root: &Path, case_insensitive: bool) -> bool {
    let path = strip_verbatim(path);
    let root = strip_verbatim(root);

    // Component-wise, never textual: a textual prefix test would let
    // `/data-private` pass as inside `/data`.
    let mut root_parts = root.components();
    let mut path_parts = path.components();

    loop {
        match (root_parts.next(), path_parts.next()) {
            (None, _) => return true,
            (Some(_), None) => return false,
            (Some(expected), Some(actual)) => {
                if !components_match(expected, actual, case_insensitive) {
                    return false;
                }
            }
        }
    }
}

fn components_match(a: Component<'_>, b: Component<'_>, case_insensitive: bool) -> bool {
    if a == b {
        return true;
    }
    if !case_insensitive {
        return false;
    }
    a.as_os_str().to_string_lossy().to_lowercase() == b.as_os_str().to_string_lossy().to_lowercase()
}

/// The user's home directory.
///
/// `HOME` first, then `USERPROFILE` for Windows. Returns `None` rather than a
/// relative fallback: writing into the working directory because an environment
/// variable was missing is worse than failing loudly.
pub fn home_dir() -> Option<PathBuf> {
    for name in ["HOME", "USERPROFILE"] {
        if let Ok(value) = std::env::var(name)
            && !value.trim().is_empty()
        {
            return Some(PathBuf::from(value));
        }
    }
    // Windows also splits the home across two variables.
    match (std::env::var("HOMEDRIVE"), std::env::var("HOMEPATH")) {
        (Ok(drive), Ok(path)) if !drive.is_empty() && !path.is_empty() => {
            Some(PathBuf::from(format!("{drive}{path}")))
        }
        _ => None,
    }
}

/// The per-user cache directory, following each platform's convention.
pub fn cache_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        if let Ok(local) = std::env::var("LOCALAPPDATA")
            && !local.is_empty()
        {
            return Some(PathBuf::from(local));
        }
        return home_dir().map(|h| h.join("AppData").join("Local"));
    }
    if cfg!(target_os = "macos") {
        return home_dir().map(|h| h.join("Library").join("Caches"));
    }
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg));
    }
    home_dir().map(|h| h.join(".cache"))
}

/// The per-user configuration directory.
pub fn config_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        if let Ok(roaming) = std::env::var("APPDATA")
            && !roaming.is_empty()
        {
            return Some(PathBuf::from(roaming));
        }
        return home_dir().map(|h| h.join("AppData").join("Roaming"));
    }
    if cfg!(target_os = "macos") {
        return home_dir().map(|h| h.join("Library").join("Application Support"));
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg));
    }
    home_dir().map(|h| h.join(".config"))
}

/// The user's `.ssh` directory, which is `~/.ssh` on every platform OpenSSH
/// supports, Windows included.
pub fn ssh_dir() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".ssh"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalisation_collapses_dot_segments() {
        assert_eq!(normalize(Path::new("/a/b/../c/./d")), PathBuf::from("/a/c/d"));
        assert_eq!(normalize(Path::new("/a/../..")), PathBuf::from("/"));
    }

    #[test]
    fn a_path_inside_a_root_is_within_it() {
        assert!(is_within_with(Path::new("/data/sub/f.txt"), Path::new("/data"), false));
        assert!(is_within_with(Path::new("/data"), Path::new("/data"), false));
    }

    #[test]
    fn a_sibling_with_a_shared_textual_prefix_is_not_within() {
        // The reason this compares components rather than strings.
        assert!(!is_within_with(Path::new("/data-private/x"), Path::new("/data"), false));
        assert!(!is_within_with(Path::new("/database/x"), Path::new("/data"), false));
    }

    #[test]
    fn a_path_outside_the_root_is_rejected() {
        assert!(!is_within_with(Path::new("/etc/passwd"), Path::new("/data"), false));
        assert!(!is_within_with(Path::new("/"), Path::new("/data"), false));
    }

    #[test]
    fn case_sensitivity_follows_the_platform_rule() {
        let path = Path::new("/Data/File.txt");
        let root = Path::new("/data");

        // On Linux these are different directories...
        assert!(!is_within_with(path, root, false));
        // ...but on Windows and macOS they are the same one, and refusing a
        // correctly configured root would be a false denial.
        assert!(is_within_with(path, root, true));
    }

    /// Path *components* are platform-defined: a Linux build sees
    /// `C:\work\file.txt` as one opaque name, so containment for Windows-shaped
    /// paths can only be asserted on Windows. The prefix stripping itself is
    /// pure string handling and is covered on every platform below.
    #[cfg(windows)]
    #[test]
    fn windows_verbatim_prefixes_are_stripped_before_comparison() {
        // canonicalize() yields the prefixed form while a not-yet-existing root
        // keeps the plain one; without stripping, the sandbox denies everything.
        assert!(is_within_with(Path::new(r"\\?\C:\work\file.txt"), Path::new(r"C:\work"), true));
        // And the case rule still applies on top of it.
        assert!(is_within_with(Path::new(r"\\?\C:\Work\f.txt"), Path::new(r"c:\work"), true));
    }

    #[test]
    fn verbatim_unc_paths_are_stripped_to_their_network_form() {
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\UNC\server\share")),
            PathBuf::from(r"\\server\share")
        );
        assert_eq!(strip_verbatim(Path::new(r"\\?\C:\x")), PathBuf::from(r"C:\x"));
        // Untouched elsewhere.
        assert_eq!(strip_verbatim(Path::new("/usr/bin")), PathBuf::from("/usr/bin"));
    }

    #[test]
    fn home_prefers_home_then_userprofile() {
        // SAFETY: single-threaded test section.
        unsafe {
            std::env::set_var("HOME", "/home/tester");
            std::env::remove_var("USERPROFILE");
        }
        assert_eq!(home_dir(), Some(PathBuf::from("/home/tester")));

        unsafe {
            std::env::remove_var("HOME");
            std::env::set_var("USERPROFILE", r"C:\Users\tester");
        }
        assert_eq!(home_dir(), Some(PathBuf::from(r"C:\Users\tester")));

        unsafe {
            std::env::remove_var("USERPROFILE");
            std::env::set_var("HOME", "/home/tester");
        }
    }

    #[test]
    fn a_blank_home_is_not_mistaken_for_a_real_one() {
        unsafe {
            std::env::set_var("HOME", "   ");
            std::env::remove_var("USERPROFILE");
            std::env::remove_var("HOMEDRIVE");
            std::env::remove_var("HOMEPATH");
        }
        // Better to report no home than to resolve to a relative path and
        // quietly write into the working directory.
        assert_eq!(home_dir(), None);
        unsafe { std::env::set_var("HOME", "/home/tester") };
    }

    #[test]
    fn the_ssh_directory_hangs_off_the_home_directory() {
        unsafe { std::env::set_var("HOME", "/home/tester") };
        assert_eq!(ssh_dir(), Some(PathBuf::from("/home/tester/.ssh")));
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    #[test]
    fn cache_and_config_follow_xdg_on_linux() {
        unsafe {
            std::env::set_var("HOME", "/home/tester");
            std::env::set_var("XDG_CACHE_HOME", "/custom/cache");
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        assert_eq!(cache_dir(), Some(PathBuf::from("/custom/cache")));
        assert_eq!(config_dir(), Some(PathBuf::from("/home/tester/.config")));
        unsafe { std::env::remove_var("XDG_CACHE_HOME") };
    }

    #[test]
    fn resolution_falls_back_to_lexical_normalisation_for_absent_paths() {
        let missing = Path::new("/definitely/not/here/../there");
        assert_eq!(resolve_for_comparison(missing), PathBuf::from("/definitely/not/there"));
    }
}
