//! Step 1 of the rewrite process (see `REWRITE_SPEC.md`): applying every embedded `.patch` file
//! to the target codebase with `git apply --3way`, fed over stdin rather than written to disk.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use super::patches::Patch;

/// Applies every patch to `root`, in order, stopping at the first failure.
pub(crate) fn apply_all(root: &Path, patches: &[Patch]) -> Result<(), String> {
    for patch in patches {
        apply_one(root, patch)?;
    }
    Ok(())
}

fn apply_one(root: &Path, patch: &Patch) -> Result<(), String> {
    let mut child = Command::new("git")
        .args(["apply", "--3way"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to run git apply for {}: {err}", patch.name))?;

    let mut stdin = child
        .stdin
        .take()
        .expect("stdin was requested as piped above");

    // The write happens on its own thread, running concurrently with `wait_with_output` below
    // (which drains stdout/stderr on the caller's behalf) rather than before it, since a patch
    // larger than the pipe buffer would otherwise deadlock: git blocked writing output nobody is
    // reading yet, us blocked writing input it has no room to accept. `stdin` is moved into the
    // thread so it's closed (signalling EOF to git) as soon as the write finishes, rather than
    // staying open until `apply_one` returns.
    let (write_result, output) = std::thread::scope(|scope| {
        let writer = scope.spawn(move || stdin.write_all(&patch.contents));
        let output = child.wait_with_output();
        (writer.join().expect("stdin-writer thread panicked"), output)
    });

    let output =
        output.map_err(|err| format!("failed to wait for git apply on {}: {err}", patch.name))?;

    if !output.status.success() {
        return Err(format!(
            "git apply --3way failed for {}:\n{}",
            patch.name,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    // Checked after the exit status: if git already failed and closed its end of the pipe early,
    // that's what caused the write to fail, and the message above is the useful one.
    write_result.map_err(|err| {
        format!(
            "git apply for {} succeeded, but writing the patch to it failed: {err}",
            patch.name
        )
    })?;

    Ok(())
}
