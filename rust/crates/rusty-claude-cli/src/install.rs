//! Rust-native `claw install` subcommand.
//!
//! Replaces the launcher-install loop in `install.sh` (the old
//! `cp` + `sed -i "6a export CLAW_CODE_ROOT=…"` per-launcher loop plus the
//! inline `claw` shim heredoc). Instead of copying the launcher scripts off
//! disk at install time, the launcher bodies are embedded at compile time via
//! `include_str!` and rewritten so that each wrapper:
//!   * keeps a single `#!/usr/bin/env bash` shebang at the top,
//!   * exports `CLAW_CODE_ROOT="<repo_root>"` immediately after the shebang,
//!   * then runs the original launcher body (with its own shebang stripped).
//!
//! The `claw` shim is synthesized to `exec` the running binary
//! (`std::env::current_exe()`), matching the heredoc the installer used to emit.

use std::path::{Path, PathBuf};

use crate::CliOutputFormat;

// Launcher bodies embedded at compile time. The path is resolved relative to
// this file (`rust/crates/rusty-claude-cli/src/install.rs`); `scripts/launchers/`
// at the repo root is four directories up. `build.rs` declares matching
// `rerun-if-changed` lines so edits to these scripts trigger a rebuild.
const LMCODE_BODY: &str = include_str!("../../../../scripts/launchers/lmcode.sh");
const OLLAMACODE_BODY: &str = include_str!("../../../../scripts/launchers/ollamacode.sh");
const OPENROUTERCODE_BODY: &str = include_str!("../../../../scripts/launchers/openroutercode.sh");
const RUN_CLAW_CODE_BODY: &str = include_str!("../../../../scripts/launchers/run-claw-code.sh");

const SHEBANG: &str = "#!/usr/bin/env bash";

/// Mapping of installed launcher destination name -> embedded source body.
/// Note `openroutercode` (the launcher was renamed from `opencode` in an
/// earlier commit; `opencode.sh` no longer exists). The synthesized `claw`
/// shim is added on top of these by [`plan_install`].
const LAUNCHERS: &[(&str, &str)] = &[
    ("lmcode", LMCODE_BODY),
    ("ollamacode", OLLAMACODE_BODY),
    ("openroutercode", OPENROUTERCODE_BODY),
    ("run-claw-code", RUN_CLAW_CODE_BODY),
];

/// A single launcher file to write: its destination basename and full body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherSpec {
    pub dest_name: String,
    pub body: String,
}

/// Look up an embedded launcher body by its source basename (e.g.
/// `"lmcode.sh"`). Kept as a small helper so the dest-name -> body mapping
/// stays in one table.
fn embedded_body(source_basename: &str) -> Option<&'static str> {
    match source_basename {
        "lmcode.sh" => Some(LMCODE_BODY),
        "ollamacode.sh" => Some(OLLAMACODE_BODY),
        "openroutercode.sh" => Some(OPENROUTERCODE_BODY),
        "run-claw-code.sh" => Some(RUN_CLAW_CODE_BODY),
        _ => None,
    }
}

/// Render a launcher wrapper: a single shebang, the `CLAW_CODE_ROOT` export,
/// an "installed by" marker, then the original launcher body with its own
/// leading shebang stripped. `name` is the source basename (e.g. `"lmcode.sh"`).
pub fn render_launcher(name: &str, repo_root: &Path) -> String {
    let body = embedded_body(name)
        .unwrap_or_else(|| panic!("no embedded launcher body for source `{name}`"));
    render_launcher_body(body, repo_root)
}

/// Pure body-rewrite used by [`render_launcher`] and [`plan_install`]: strip a
/// leading `#!/usr/bin/env bash` line from `body`, then prepend our own shebang
/// + `CLAW_CODE_ROOT` export.
fn render_launcher_body(body: &str, repo_root: &Path) -> String {
    let stripped = strip_leading_shebang(body);
    let root = repo_root.display();
    format!(
        "{SHEBANG}\n# Installed by `claw install`\nexport CLAW_CODE_ROOT=\"{root}\"\n{stripped}"
    )
}

/// Remove a single leading `#!/usr/bin/env bash` line (and its trailing
/// newline) from a launcher body if present. Any other leading content is
/// returned unchanged.
fn strip_leading_shebang(body: &str) -> &str {
    if let Some(rest) = body.strip_prefix(SHEBANG) {
        // Drop the newline that terminated the shebang line, if any.
        rest.strip_prefix('\n').unwrap_or(rest)
    } else {
        body
    }
}

/// Render the `claw` shim: a wrapper that execs the built `claw` binary,
/// forwarding all arguments. Mirrors the heredoc `install.sh` used to write.
fn render_claw_shim(claw_bin: &Path) -> String {
    let bin = claw_bin.display();
    format!("{SHEBANG}\nexec \"{bin}\" \"$@\"\n")
}

/// Compute the full set of launcher files to install: the embedded launchers
/// (rewritten with `CLAW_CODE_ROOT`) plus the synthesized `claw` shim. Pure:
/// performs no filesystem I/O.
pub fn plan_install(_install_dir: &Path, repo_root: &Path, claw_bin: &Path) -> Vec<LauncherSpec> {
    let mut specs: Vec<LauncherSpec> = LAUNCHERS
        .iter()
        .map(|(dest_name, body)| LauncherSpec {
            dest_name: (*dest_name).to_string(),
            body: render_launcher_body(body, repo_root),
        })
        .collect();
    specs.push(LauncherSpec {
        dest_name: "claw".to_string(),
        body: render_claw_shim(claw_bin),
    });
    specs
}

/// Default install directory: `$HOME/.local/bin`.
fn default_install_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    PathBuf::from(home).join(".local").join("bin")
}

/// Walk up from a starting directory looking for a repo root that contains both
/// `rust/Cargo.toml` and `scripts/launchers/`.
fn find_repo_root_from(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if dir.join("rust").join("Cargo.toml").is_file()
            && dir.join("scripts").join("launchers").is_dir()
        {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

/// Detect the repo root by walking up from the running binary's directory and,
/// failing that, the current working directory. The `--repo-root` flag
/// overrides this in [`run_install`].
fn detect_repo_root() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if let Some(root) = find_repo_root_from(dir) {
                return Some(root);
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(root) = find_repo_root_from(&cwd) {
            return Some(root);
        }
    }
    None
}

/// Write a launcher body to `dest`, creating parent directories as needed and
/// marking the file executable (mode 0o755) on unix.
fn write_launcher(dest: &Path, body: &str) -> std::io::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(dest, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// Resolve the install dir + repo root, write every launcher (plus the `claw`
/// shim), and report what was installed. Honors `output_format` for the final
/// summary (text or a minimal JSON object).
pub fn run_install(
    install_dir: Option<PathBuf>,
    repo_root: Option<PathBuf>,
    force: bool,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let _ = force; // Files are always (over)written; `--force` reserved for future guards.

    let install_dir = install_dir.unwrap_or_else(default_install_dir);
    let repo_root = match repo_root.or_else(detect_repo_root) {
        Some(root) => root,
        None => {
            return Err(Box::<dyn std::error::Error>::from(
                "could not locate the claw repo root (expected a directory containing both \
                 `rust/Cargo.toml` and `scripts/launchers/`). Pass `--repo-root <path>`.",
            ));
        }
    };
    let claw_bin = std::env::current_exe()?;

    let specs = plan_install(&install_dir, &repo_root, &claw_bin);

    std::fs::create_dir_all(&install_dir)?;

    let mut installed: Vec<PathBuf> = Vec::with_capacity(specs.len());
    for spec in &specs {
        let dest = install_dir.join(&spec.dest_name);
        write_launcher(&dest, &spec.body)?;
        installed.push(dest);
    }

    match output_format {
        CliOutputFormat::Text => {
            for dest in &installed {
                println!("installed {}", dest.display());
            }
            println!(
                "{} launchers installed to {}",
                installed.len(),
                install_dir.display()
            );
            if !path_contains_dir(&install_dir) {
                eprintln!(
                    "warning: {} is not on your PATH; add it to your shell profile:",
                    install_dir.display()
                );
                eprintln!("  export PATH=\"{}:$PATH\"", install_dir.display());
            }
        }
        CliOutputFormat::Json => {
            let installed_strs: Vec<String> =
                installed.iter().map(|p| p.display().to_string()).collect();
            let payload = serde_json::json!({
                "install_dir": install_dir.display().to_string(),
                "repo_root": repo_root.display().to_string(),
                "claw_bin": claw_bin.display().to_string(),
                "installed": installed_strs,
            });
            println!("{}", serde_json::to_string_pretty(&payload)?);
        }
    }

    Ok(())
}

/// Whether `dir` appears as a component of the `PATH` environment variable.
fn path_contains_dir(dir: &Path) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|p| p == dir))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("rusty-claude-cli-install-{label}-{nanos}-{n}"))
    }

    #[test]
    fn render_launcher_injects_claw_code_root() {
        let rendered = render_launcher("openroutercode.sh", Path::new("/some/repo"));
        // CLAW_CODE_ROOT export is present right after the shebang block.
        assert!(
            rendered.contains("export CLAW_CODE_ROOT=\"/some/repo\""),
            "missing CLAW_CODE_ROOT export:\n{rendered}"
        );
        // A known line from the openrouter launcher body survives.
        assert!(
            rendered.contains("https://openrouter.ai/api/v1"),
            "missing openrouter body content:\n{rendered}"
        );
        // The export comes before the embedded body content.
        let export_idx = rendered
            .find("export CLAW_CODE_ROOT=")
            .expect("export present");
        let body_idx = rendered
            .find("https://openrouter.ai/api/v1")
            .expect("body present");
        assert!(export_idx < body_idx, "export should precede body");
    }

    #[test]
    fn render_launcher_strips_inner_shebang() {
        let rendered = render_launcher("lmcode.sh", Path::new("/some/repo"));
        let shebang_count = rendered.matches(SHEBANG).count();
        assert_eq!(
            shebang_count, 1,
            "expected exactly one shebang, found {shebang_count}:\n{rendered}"
        );
        assert!(
            rendered.starts_with(SHEBANG),
            "shebang must be the first line:\n{rendered}"
        );
    }

    #[test]
    fn render_claw_shim_execs_binary() {
        let shim = render_claw_shim(Path::new("/opt/claw/bin/claw"));
        assert!(
            shim.contains("exec \"/opt/claw/bin/claw\" \"$@\""),
            "shim should exec the binary:\n{shim}"
        );
        assert!(shim.starts_with(SHEBANG), "shim needs a shebang:\n{shim}");
    }

    #[test]
    fn plan_install_maps_all_launcher_names() {
        let specs = plan_install(
            Path::new("/tmp/bin"),
            Path::new("/some/repo"),
            Path::new("/some/repo/rust/target/debug/claw"),
        );
        let names: std::collections::BTreeSet<String> =
            specs.iter().map(|s| s.dest_name.clone()).collect();
        let expected: std::collections::BTreeSet<String> = [
            "claw",
            "lmcode",
            "ollamacode",
            "openroutercode",
            "run-claw-code",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn default_install_dir_is_local_bin() {
        // Mutating HOME is process-global; the assertion below only depends on
        // the suffix, so a transient overlap with another test is harmless, but
        // restore HOME afterward to avoid surprising later tests.
        let previous = std::env::var_os("HOME");
        let tmp = unique_temp_dir("home");
        std::env::set_var("HOME", &tmp);
        let dir = default_install_dir();
        match previous {
            Some(val) => std::env::set_var("HOME", val),
            None => std::env::remove_var("HOME"),
        }
        assert!(
            dir.ends_with("bin"),
            "expected dir ending in bin, got {}",
            dir.display()
        );
        assert!(
            dir.ends_with(PathBuf::from(".local").join("bin")),
            "expected dir ending in .local/bin, got {}",
            dir.display()
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_install_writes_executable_files() {
        use std::os::unix::fs::PermissionsExt;

        let install_dir = unique_temp_dir("install");
        // repo_root is supplied explicitly so detection/HOME are irrelevant.
        run_install(
            Some(install_dir.clone()),
            Some(PathBuf::from("/some/repo")),
            false,
            CliOutputFormat::Text,
        )
        .expect("install should succeed");

        for name in [
            "claw",
            "lmcode",
            "ollamacode",
            "openroutercode",
            "run-claw-code",
        ] {
            let path = install_dir.join(name);
            assert!(path.is_file(), "missing installed file: {}", path.display());
            let mode = std::fs::metadata(&path)
                .expect("metadata should load")
                .permissions()
                .mode();
            assert!(
                mode & 0o111 != 0,
                "file {} is not executable (mode {:o})",
                path.display(),
                mode
            );
        }

        // Cleanup.
        let _ = std::fs::remove_dir_all(&install_dir);
    }
}
