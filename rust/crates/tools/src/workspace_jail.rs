//! Symlink-safe workspace jail helpers (ported from upstream
//! `claw-analog/src/lib.rs`).
//!
//! These validate that a path stays inside an allowed workspace root, so a
//! sandboxed sub-agent cannot escape via `..`, an absolute path, or a symlink
//! that points outside the root.
//!
//! Currently these are ported + unit-tested but NOT yet wired into the file
//! tools. `SubagentToolExecutor` dispatches to the shared `execute_tool*`
//! path, which resolves paths against the process cwd; enforcing the jail
//! there would require threading a root through that dispatch.
//!
//! TODO: enforce in isolated runs — wire `assert_workspace_path` into the
//! read/write/edit tool handlers when the sub-agent runs against a dedicated
//! worktree root, rather than the current cwd-relative resolution.

use std::path::{Component, Path, PathBuf};

/// Reject a relative path that is unsafe to join onto a workspace root.
///
/// Rejects:
/// - absolute paths (`/etc/passwd`, `C:\...`),
/// - any path containing a `..` component,
/// - paths containing a backslash (Windows-style separators, which can slip
///   past Unix component parsing).
///
/// # Errors
/// Returns `Err` with a human-readable reason when the path is unsafe.
pub fn validate_rel_path(rel: &str) -> Result<(), String> {
    if rel.contains('\\') {
        return Err(format!("path `{rel}` contains a backslash"));
    }
    let path = Path::new(rel);
    if path.is_absolute() {
        return Err(format!("path `{rel}` is absolute"));
    }
    for component in path.components() {
        match component {
            Component::ParentDir => {
                return Err(format!("path `{rel}` contains a `..` component"));
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(format!("path `{rel}` is not relative"));
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(())
}

/// Assert that `candidate` resolves to a location inside `root`, following
/// symlinks where possible.
///
/// For existing paths this canonicalizes both `root` and `candidate` (so a
/// symlink pointing outside the root is rejected). For a not-yet-existing
/// path, it canonicalizes the nearest existing ancestor and re-appends the
/// remaining components, then checks `starts_with(root)`.
///
/// # Errors
/// Returns `Err` when the path canonicalizes (or would canonicalize) to a
/// location outside `root`, or when `root` itself cannot be canonicalized.
pub fn assert_workspace_path(root: &Path, candidate: &Path) -> Result<PathBuf, String> {
    let canonical_root = root.canonicalize().map_err(|error| {
        format!(
            "cannot canonicalize workspace root `{}`: {error}",
            root.display()
        )
    })?;

    let resolved = canonicalize_allowing_missing(candidate)?;

    if resolved.starts_with(&canonical_root) {
        Ok(resolved)
    } else {
        Err(format!(
            "path `{}` escapes workspace root `{}`",
            resolved.display(),
            canonical_root.display()
        ))
    }
}

/// Canonicalize `path` when it exists; otherwise canonicalize the nearest
/// existing ancestor and re-append the trailing (missing) components.
fn canonicalize_allowing_missing(path: &Path) -> Result<PathBuf, String> {
    if let Ok(canonical) = path.canonicalize() {
        return Ok(canonical);
    }
    // Walk up to the nearest existing ancestor.
    let mut existing = path;
    let mut trailing: Vec<&std::ffi::OsStr> = Vec::new();
    while let Some(parent) = existing.parent() {
        if let Some(name) = existing.file_name() {
            trailing.push(name);
        }
        existing = parent;
        if existing.exists() {
            break;
        }
    }
    let base = if existing.as_os_str().is_empty() {
        std::env::current_dir().map_err(|error| format!("cannot resolve cwd: {error}"))?
    } else {
        existing
            .canonicalize()
            .map_err(|error| format!("cannot canonicalize `{}`: {error}", existing.display()))?
    };
    let mut resolved = base;
    for name in trailing.into_iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rel_path_rejects_parent_dir() {
        assert!(validate_rel_path("../secrets").is_err());
        assert!(validate_rel_path("a/../../b").is_err());
        assert!(validate_rel_path("..").is_err());
    }

    #[test]
    fn validate_rel_path_rejects_absolute() {
        assert!(validate_rel_path("/etc/passwd").is_err());
    }

    #[test]
    fn validate_rel_path_rejects_backslash() {
        assert!(validate_rel_path("a\\b").is_err());
        assert!(validate_rel_path("..\\windows").is_err());
    }

    #[test]
    fn validate_rel_path_accepts_safe_relative() {
        assert!(validate_rel_path("src/lib.rs").is_ok());
        assert!(validate_rel_path("./src/lib.rs").is_ok());
        assert!(validate_rel_path("a/b/c.txt").is_ok());
    }

    #[test]
    fn assert_workspace_path_accepts_in_root() {
        let root = std::env::temp_dir().join(format!(
            "claw-jail-ok-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("sub")).expect("mkdir");
        let target = root.join("sub/file.txt");
        std::fs::write(&target, "hi").expect("write");

        let resolved = assert_workspace_path(&root, &target).expect("in-root path");
        assert!(resolved.starts_with(root.canonicalize().unwrap()));

        // Non-existent file under root is allowed (parent canonicalizes inside).
        let missing = root.join("sub/new.txt");
        assert!(assert_workspace_path(&root, &missing).is_ok());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn assert_workspace_path_rejects_outside_root() {
        let base = std::env::temp_dir().join(format!(
            "claw-jail-esc-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).expect("mkdir root");
        std::fs::create_dir_all(&outside).expect("mkdir outside");
        let outside_file = outside.join("secret.txt");
        std::fs::write(&outside_file, "secret").expect("write");

        assert!(assert_workspace_path(&root, &outside_file).is_err());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn assert_workspace_path_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let base = std::env::temp_dir().join(format!(
            "claw-jail-sym-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).expect("mkdir root");
        std::fs::create_dir_all(&outside).expect("mkdir outside");
        std::fs::write(outside.join("target.txt"), "secret").expect("write");

        // root/link -> outside/target.txt
        let link = root.join("link");
        symlink(outside.join("target.txt"), &link).expect("symlink");

        // The symlink lives inside root, but resolves outside it.
        assert!(assert_workspace_path(&root, &link).is_err());

        let _ = std::fs::remove_dir_all(&base);
    }
}
