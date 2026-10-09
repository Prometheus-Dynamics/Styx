//! A test's scratch directory under the system temp dir, removed when the test ends (pass or
//! fail) unless `STYX_KEEP_TEST_DIRS` is set.

use std::ops::Deref;
use std::path::{Path, PathBuf};

pub(crate) struct TmpDir(PathBuf);

/// An empty `styx-record-<name>-<pid>` directory.
pub(crate) fn tmp(name: &str) -> TmpDir {
    let dir = std::env::temp_dir().join(format!("styx-record-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    TmpDir(dir)
}

impl Deref for TmpDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TmpDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        if std::env::var_os("STYX_KEEP_TEST_DIRS").is_none() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
