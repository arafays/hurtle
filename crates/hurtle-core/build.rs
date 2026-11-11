//! Stamp the build with its git commit (`HURTLE_COMMIT`), so the app can say
//! exactly which build is running. Builds outside a git checkout (a source
//! tarball) just have none.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    // Re-stamp on a commit or checkout only: watching the sources too would
    // rebuild the engine on every edit.
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        for f in ["HEAD", "index", "refs"] {
            println!("cargo:rerun-if-changed={git_dir}/{f}");
        }
    }
    let Some(commit) = git(&["rev-parse", "--short=7", "HEAD"]) else {
        return;
    };
    println!("cargo:rustc-env=HURTLE_COMMIT={commit}");
}
