use std::path::{Component, Path, PathBuf};

use crate::CoreError;

/// The single gate between tool arguments and the filesystem.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, CoreError> {
        let root = std::fs::canonicalize(root.as_ref())?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Canonicalize `p` relative to the root and reject anything that escapes.
    ///
    /// The path need not exist: we canonicalize the nearest existing ancestor
    /// and re-append the remaining components. Symlinks are resolved, so a link
    /// inside the root that points outside it is rejected.
    pub fn resolve(&self, p: impl AsRef<Path>) -> Result<PathBuf, CoreError> {
        let p = p.as_ref();
        let joined = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        };

        // Walk up to the nearest existing ancestor, canonicalize it, then
        // re-append the tail. `canonicalize` fails on non-existent paths.
        let mut existing = joined.as_path();
        let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
        let canonical_base = loop {
            match std::fs::canonicalize(existing) {
                Ok(c) => break c,
                Err(_) => match existing.parent() {
                    Some(parent) => {
                        if let Some(name) = existing.file_name() {
                            tail.push(name);
                        }
                        existing = parent;
                    }
                    None => return Err(CoreError::PathEscape(joined)),
                },
            }
        };

        let mut out = canonical_base;
        for name in tail.into_iter().rev() {
            out.push(name);
        }

        // Reject `..` that survived (only possible in the non-existent tail).
        if out.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(CoreError::PathEscape(out));
        }
        if !out.starts_with(&self.root) {
            return Err(CoreError::PathEscape(out));
        }
        Ok(out)
    }

    /// A uniquely-named directory under the root, removed on drop.
    pub fn temp_scope(&self, prefix: &str) -> Result<TempScope, CoreError> {
        let dir = self.root.join(format!(
            ".compact-mcp-tmp/{prefix}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir)?;
        Ok(TempScope { dir })
    }
}

/// A scratch directory inside the workspace root. Removed on drop.
#[derive(Debug)]
pub struct TempScope {
    dir: PathBuf,
}

impl TempScope {
    pub fn path(&self) -> &Path {
        &self.dir
    }

    pub fn write_file(&self, name: &str, contents: &str) -> Result<PathBuf, CoreError> {
        if name.contains('/') || name.contains('\\') || name == ".." {
            return Err(CoreError::InvalidArgs(format!(
                "bad temp file name: {name}"
            )));
        }
        let p = self.dir.join(name);
        std::fs::write(&p, contents)?;
        Ok(p)
    }
}

impl Drop for TempScope {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> (tempfile::TempDir, Workspace) {
        let d = tempfile::tempdir().unwrap();
        let w = Workspace::new(d.path()).unwrap();
        (d, w)
    }

    #[test]
    fn resolves_a_path_inside_the_root() {
        let (d, w) = ws();
        std::fs::write(d.path().join("a.compact"), "x").unwrap();
        let p = w.resolve("a.compact").unwrap();
        assert!(p.starts_with(w.root()));
    }

    #[test]
    fn rejects_dotdot_escape() {
        let (_d, w) = ws();
        assert!(matches!(
            w.resolve("../evil"),
            Err(CoreError::PathEscape(_))
        ));
    }

    #[test]
    fn rejects_absolute_path_outside_root() {
        let (_d, w) = ws();
        assert!(matches!(
            w.resolve("/etc/passwd"),
            Err(CoreError::PathEscape(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_pointing_outside_root() {
        let (d, w) = ws();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "s").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), d.path().join("link")).unwrap();
        assert!(matches!(w.resolve("link"), Err(CoreError::PathEscape(_))));
    }

    #[test]
    fn resolves_a_path_that_does_not_exist_yet() {
        // `target_dir` for a build has not been created yet. The nearest
        // existing ancestor must still be inside the root.
        let (_d, w) = ws();
        let p = w.resolve("build/out").unwrap();
        assert!(p.starts_with(w.root()));
    }

    #[test]
    fn temp_scope_lives_inside_the_root_and_is_removed_on_drop() {
        let (_d, w) = ws();
        let path = {
            let scope = w.temp_scope("probe").unwrap();
            assert!(scope.path().starts_with(w.root()));
            scope
                .write_file("a.compact", "pragma language_version >= 0.23;")
                .unwrap();
            scope.path().to_path_buf()
        };
        assert!(!path.exists(), "TempScope must clean up on drop");
    }

    proptest::proptest! {
        #[test]
        fn resolve_never_escapes_the_root(segments in proptest::collection::vec(
            proptest::string::string_regex("[a-zA-Z0-9._/-]{0,8}").unwrap(), 0..6
        )) {
            let d = tempfile::tempdir().unwrap();
            let w = Workspace::new(d.path()).unwrap();
            let joined = segments.join("/");
            if let Ok(p) = w.resolve(&joined) {
                proptest::prop_assert!(p.starts_with(w.root()), "escaped: {joined} -> {p:?}");
            }
        }
    }
}
