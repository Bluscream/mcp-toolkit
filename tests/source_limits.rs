//! Structural limits on the source itself.
//!
//! Function length is enforced by clippy (`too-many-lines-threshold = 100`).
//! File length has no clippy equivalent, so it is checked here: a module past
//! a thousand lines is doing too much and should be split, which is the same
//! reasoning that produced the `foo.rs` + `foo/tests.rs` layout used
//! throughout.

use std::path::{Path, PathBuf};

const MAX_LINES: usize = 1000;

#[test]
fn no_source_file_exceeds_the_line_limit() {
    let mut offenders = Vec::new();
    for path in rust_sources() {
        let Ok(content) = std::fs::read_to_string(&path) else { continue };
        let lines = content.lines().count();
        if lines > MAX_LINES {
            offenders.push(format!("  {} — {lines} lines", path.display()));
        }
    }

    assert!(
        offenders.is_empty(),
        "these files exceed the {MAX_LINES}-line limit; split them:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_check_actually_finds_the_sources() {
    // Guards against the walk silently matching nothing, which would make the
    // limit above vacuously true.
    let sources = rust_sources();
    assert!(sources.len() >= 4, "expected to find the crate's modules, got {}", sources.len());
}

fn rust_sources() -> Vec<PathBuf> {
    let mut found = Vec::new();
    for root in ["src", "tests"] {
        collect(Path::new(root), &mut found);
    }
    found
}

fn collect(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, found);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
}
