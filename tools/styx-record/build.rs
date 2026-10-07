//! Embeds the Styx commit the recorder is built from (`STYX_RECORD_COMMIT`): `$STYX_COMMIT`
//! when set (builds outside a git checkout), else `git rev-parse HEAD` with `-dirty` when the
//! work tree has changes, else `unknown`.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-env-changed=STYX_COMMIT");
    let commit = std::env::var("STYX_COMMIT")
        .ok()
        .filter(|c| !c.is_empty())
        .or_else(|| {
            let head = git(&["rev-parse", "HEAD"])?;
            let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
                .is_some_and(|s| !s.is_empty());
            Some(if dirty { format!("{head}-dirty") } else { head })
        })
        .unwrap_or_else(|| "unknown".into());
    // Rebuild when HEAD moves (a commit, a checkout).
    for path in ["HEAD", "index"] {
        if let Some(p) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={p}");
        }
    }
    if let Some(head_ref) = git(&["symbolic-ref", "-q", "HEAD"])
        && let Some(p) = git(&["rev-parse", "--git-path", &head_ref])
    {
        println!("cargo:rerun-if-changed={p}");
    }
    println!("cargo:rustc-env=STYX_RECORD_COMMIT={commit}");
}
