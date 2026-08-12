use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};

/// Resolve a sibling Cargo binary for an integration test without falling back
/// to a repository-level target directory that may contain an older build.
///
/// Cargo places integration test executables in `<target>/<profile>/deps`, so
/// walking up from the current test executable selects the exact target
/// directory/profile used by the current test invocation (including a custom
/// `CARGO_TARGET_DIR`). Explicit overrides remain available for packaging and
/// cross-target test harnesses, but are validated before spawning.
pub fn binary(env_var: &str, name: &str) -> Result<PathBuf> {
    let path = match std::env::var_os(env_var) {
        Some(value) => PathBuf::from(value),
        None => {
            let test_exe =
                std::env::current_exe().context("determine integration test executable path")?;
            let profile_dir = test_exe.parent().and_then(Path::parent).context(
                "integration test executable is not inside a Cargo target profile directory",
            )?;
            profile_dir.join(name)
        }
    };

    ensure!(
        path.is_file(),
        "required {name} test binary not found at {}. Build it with `cargo build -p mux_server -p z3rm` or set {env_var} to the matching binary",
        path.display()
    );
    Ok(path)
}
