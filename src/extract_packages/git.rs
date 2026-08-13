//! Gives every package its own git history: composer.json deliberately carries no `version` field,
//! since Composer derives `dev-main` from a real git branch instead, so a package that never gets
//! committed can't be depended on by version at all.

use std::path::Path;
use std::process::Command;

/// Initialises `package_dir` as its own git repository on a `main` branch and commits everything
/// currently in it. Uses whatever git identity (`user.name`/`user.email`) is already configured in
/// the environment running this command — this is a development tool, not something that needs a
/// fixed, reproducible committer identity of its own.
pub(super) fn init_and_commit(package_dir: &Path) -> Result<(), String> {
    run(package_dir, &["init", "--quiet", "-b", "main"])?;
    run(package_dir, &["add", "--all"])?;
    run(
        package_dir,
        &["commit", "--quiet", "-m", "Initial package import"],
    )?;
    Ok(())
}

fn run(dir: &Path, args: &[&str]) -> Result<(), String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|err| format!("failed to run `git {}`: {err}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "`git {}` failed in {}: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "scan-moodle-git-test-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn commits_every_file_to_a_main_branch() {
        let dir = temp_dir("basic");
        fs::write(dir.join("composer.json"), "{}\n").unwrap();

        init_and_commit(&dir).unwrap();

        let branch = Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&branch.stdout).trim(), "main");

        let status = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&status.stdout).trim().is_empty(),
            "composer.json should already be committed, not left pending"
        );

        fs::remove_dir_all(&dir).ok();
    }
}
