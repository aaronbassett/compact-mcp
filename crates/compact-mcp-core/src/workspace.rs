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

        // Walk up to the nearest existing ancestor and canonicalize it
        // (resolving symlinks). `canonicalize` fails on non-existent paths, so
        // we peel components off the end and retry until something exists.
        // Use `components().next_back()` rather than `file_name()`: the latter
        // returns `None` for a trailing `..`, which would silently drop the
        // component and let a traversal collapse into an unrelated in-root path.
        let mut existing = joined.as_path();
        let mut tail = Vec::new();
        let canonical_base = loop {
            match std::fs::canonicalize(existing) {
                Ok(c) => break c,
                Err(_) => match (existing.components().next_back(), existing.parent()) {
                    (Some(last), Some(parent)) => {
                        tail.push(last);
                        existing = parent;
                    }
                    _ => return Err(CoreError::PathEscape(joined)),
                },
            }
        };

        // Re-append the non-existent tail, resolving `.`/`..` lexically against
        // the canonical base. A `..` that pops above the base escapes the root.
        let mut out = canonical_base;
        for component in tail.into_iter().rev() {
            match component {
                Component::Normal(name) => out.push(name),
                Component::CurDir => {}
                Component::ParentDir => {
                    if !out.pop() {
                        return Err(CoreError::PathEscape(joined));
                    }
                }
                // A root or prefix can only be the first canonicalized
                // component, never part of the non-existent tail.
                Component::RootDir | Component::Prefix(_) => {
                    return Err(CoreError::PathEscape(joined));
                }
            }
        }

        if !out.starts_with(&self.root) {
            return Err(CoreError::PathEscape(out));
        }
        Ok(out)
    }

    /// A uniquely-named directory under the root, removed on drop.
    pub fn temp_scope(&self, prefix: &str) -> Result<TempScope, CoreError> {
        // The prefix is joined onto a trusted base, so it must not traverse.
        // `../../x` would otherwise place the scratch dir (and its later
        // `remove_dir_all`) above the root.
        if !is_single_normal_component(prefix) {
            return Err(CoreError::InvalidArgs(format!(
                "bad temp scope prefix: {prefix}"
            )));
        }
        let dir = self
            .root
            .join(".compact-mcp-tmp")
            .join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        Ok(TempScope { dir })
    }
}

/// Returns `true` if `name` is exactly one normal path component: no
/// separators, no `.` or `..`, not empty, not absolute, and (on Windows) no
/// drive-relative prefix such as `C:`. Such names can be joined onto a trusted
/// base without escaping or redirecting it.
pub(crate) fn is_single_normal_component(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
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
        // A single `Component::Normal` check subsumes separators, `.`, `..`,
        // the empty name, absolute paths, and Windows drive prefixes like `C:`,
        // any of which would let `join` escape or replace the scope directory.
        if !is_single_normal_component(name) {
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
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            tracing::warn!(
                path = %self.dir.display(),
                %error,
                "failed to remove temp scope directory on drop"
            );
        }
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

    #[test]
    fn temp_scope_rejects_a_traversing_prefix() {
        // A prefix must be a single path component. `../../x` would place the
        // scratch directory (and its later `remove_dir_all`) above the root.
        let (_d, w) = ws();
        assert!(matches!(
            w.temp_scope("../../x"),
            Err(CoreError::InvalidArgs(_))
        ));
    }

    #[test]
    fn write_file_rejects_non_component_names() {
        // Anything that is not exactly one normal component is rejected:
        // separators, traversal, current-dir, and the empty name all escape or
        // redirect the join.
        let (_d, w) = ws();
        let scope = w.temp_scope("probe").unwrap();
        for bad in ["../escape", "a/b", "..", ".", ""] {
            assert!(
                matches!(scope.write_file(bad, "x"), Err(CoreError::InvalidArgs(_))),
                "should reject name {bad:?}"
            );
        }
    }

    #[test]
    fn dotdot_that_escapes_via_a_nonexistent_prefix_is_rejected() {
        // `build` does not exist, so the whole tail is lexical. The `..`
        // segments must be resolved (not silently dropped) so the escape is
        // caught instead of collapsing to `<root>/build/evil`.
        let (_d, w) = ws();
        assert!(matches!(
            w.resolve("build/../../evil"),
            Err(CoreError::PathEscape(_))
        ));
    }

    #[test]
    fn dotdot_within_the_root_resolves_lexically() {
        // A `..` that stays inside the root must resolve to the correct target,
        // not be dropped: `build/../out` is `<root>/out`.
        let (_d, w) = ws();
        let p = w.resolve("build/../out").unwrap();
        assert_eq!(p, w.root().join("out"));
        assert!(p.starts_with(w.root()));
    }

    proptest::proptest! {
        #[test]
        fn resolve_never_escapes_the_root(segments in proptest::collection::vec(
            proptest::string::string_regex("[a-b./]{0,8}").unwrap(), 0..6
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
