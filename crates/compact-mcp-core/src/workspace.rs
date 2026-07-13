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
                Err(_) => {
                    // `canonicalize` fails both for a path that does not exist
                    // YET and for a symlink it cannot resolve (a dangling link,
                    // or one whose target has left the tree). Distinguish them
                    // with an `lstat`: a component that exists AS a symlink was
                    // never part of a canonical path, so treating it as an
                    // absent tail component would hand back an in-root-looking
                    // path that a later write would follow outside the root.
                    // Reject it — this is the tail check that must be more than
                    // lexical (issue #9). A symlink whose target still exists is
                    // already caught below by the `starts_with(root)` guard,
                    // because `canonicalize` resolves it; only the unresolvable
                    // (dangling) case reaches here.
                    if std::fs::symlink_metadata(existing)
                        .map(|m| m.file_type().is_symlink())
                        .unwrap_or(false)
                    {
                        return Err(CoreError::PathEscape(joined));
                    }
                    match (existing.components().next_back(), existing.parent()) {
                        (Some(last), Some(parent)) => {
                            tail.push(last);
                            existing = parent;
                        }
                        _ => return Err(CoreError::PathEscape(joined)),
                    }
                }
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

    /// Re-check, immediately before handing `vetted` to a writer (or to a
    /// subprocess that writes), that it still denotes the same in-root location
    /// [`Workspace::resolve`] approved. This is the stronger of the two
    /// TOCTOU re-checks — use it wherever a [`Workspace`] is in hand.
    ///
    /// It layers the root-free symlink-swap guard ([`assert_no_symlink_swap`])
    /// with a root-aware belt: the *real* (symlink-resolved) location of the
    /// nearest existing ancestor must still sit inside the canonical root. The
    /// guard catches a symlink planted as a component of `vetted`; the belt
    /// additionally catches an ancestor directory redirected out of the root
    /// even when the redirected subpath happens to exist (which the guard alone
    /// would walk straight through). Because `self.root` is itself canonical,
    /// the belt tolerates benign symlinks *above* the root (e.g. macOS's
    /// `/var` → `/private/var`).
    ///
    /// This closes the check-then-use window between `resolve` and the write: a
    /// symlink planted at (or above) `vetted` after it was resolved is refused
    /// here rather than silently followed out of the root.
    pub fn revalidate_before_write(&self, vetted: &Path) -> Result<(), CoreError> {
        assert_no_symlink_swap(vetted)?;

        // Belt: canonicalize the nearest existing ancestor NOW and require its
        // real path to stay under the canonical root. `assert_no_symlink_swap`
        // has already rejected any symlink *component* (including a dangling
        // one), so reaching here means every existing component is a real
        // file/dir; this catches the residual case where a real ancestor dir was
        // swapped for a symlink whose out-of-root target already existed.
        let mut existing = vetted;
        let real = loop {
            match std::fs::canonicalize(existing) {
                Ok(c) => break c,
                Err(_) => match existing.parent() {
                    Some(parent) => existing = parent,
                    None => return Err(CoreError::PathEscape(vetted.to_path_buf())),
                },
            }
        };
        if !real.starts_with(&self.root) {
            return Err(CoreError::PathEscape(vetted.to_path_buf()));
        }
        Ok(())
    }
}

/// Re-assert, immediately before a write, that `vetted` — a path previously
/// returned by [`Workspace::resolve`] — has not had a symlink swapped into it
/// since it was resolved.
///
/// `resolve` canonicalises the existing prefix and leaves the non-existent tail
/// lexically clean, so at resolve time no component of `vetted` down to its
/// nearest existing ancestor is a symlink. Here we re-walk from `vetted` up to
/// that nearest currently-existing ancestor and reject if the first component
/// that exists is a symlink — `lstat` does not follow a path's final component,
/// so a freshly-planted link (including a *dangling* one, which `canonicalize`
/// cannot resolve and would otherwise be mistaken for an absent tail component)
/// is seen as a link. This is exactly the mechanism the deep-dive review
/// proved: a not-yet-existing `target_dir` planted as a symlink, and an in-root
/// file swapped for a symlink — both are the final existing component, both are
/// caught here.
///
/// This deliberately needs no workspace root, so it serves call sites holding
/// only a resolved `PathBuf` — notably the compile path, whose `CompileRequest`
/// carries no [`Workspace`]. Because the walk stops at the *nearest existing
/// ancestor* (not the filesystem root), it never inspects — and so never trips
/// over — benign symlinks above the workspace (e.g. macOS's `/var`).
///
/// Residuals (defense-in-depth, not a full close):
/// - Point-in-time: a link swapped in *after* this returns but *before* the
///   writer opens the path is not caught. For our compiler-driven writes the
///   remaining window is the spawn-to-open gap (down from the full compile
///   timeout), and the compiler's own `open`s inside the target are not under
///   our `O_NOFOLLOW` control.
/// - Root-free: an *ancestor directory* swapped for a symlink whose out-of-root
///   target already exists is not caught here (the existing subpath is a real
///   dir, not a symlink). Call sites holding a [`Workspace`] should prefer
///   [`Workspace::revalidate_before_write`], whose root-aware belt closes that
///   case; the compile path's residual is documented at its call site.
pub fn assert_no_symlink_swap(vetted: &Path) -> Result<(), CoreError> {
    let mut existing = vetted;
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(md) if md.file_type().is_symlink() => {
                // A symlink where `resolve` left either a canonical
                // (non-symlink) component or a not-yet-existing tail: it was
                // planted after the fact. `symlink_metadata` (lstat) reports the
                // link itself, so this fires even for a dangling link.
                return Err(CoreError::PathEscape(vetted.to_path_buf()));
            }
            // Nearest existing ancestor, and it is a real file/dir — nothing was
            // swapped into the part of `vetted` that exists.
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => match existing.parent() {
                Some(parent) => existing = parent,
                None => return Err(CoreError::PathEscape(vetted.to_path_buf())),
            },
            Err(e) => return Err(CoreError::Io(e)),
        }
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
        write_no_follow(&p, contents.as_bytes())?;
        Ok(p)
    }
}

/// Write `contents` to `path`, creating or truncating it, but refusing to follow
/// a final-component symlink. This is a write WE perform, so we close it with
/// `O_NOFOLLOW` directly (issue #9): if the name has been swapped for a symlink
/// since the scope was created, the `open` fails (ELOOP) rather than following
/// the link and writing through it. The name is a single component joined onto a
/// freshly-created UUID scratch dir, so a pre-planted link there is far-fetched
/// — but the guard is cheap and unconditional. On non-unix targets there is no
/// `O_NOFOLLOW`; fall back to a plain write.
fn write_no_follow(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        f.write_all(contents)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
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

    // --- Issue #9: TOCTOU re-validation between `resolve` and the write. ---

    #[test]
    fn assert_no_symlink_swap_passes_for_a_legit_nonexistent_tail() {
        // A `target_dir` that does not exist yet (the common compile case) must
        // pass the re-check: nothing was swapped in.
        let (_d, w) = ws();
        let vetted = w.resolve("build/out").unwrap();
        assert!(assert_no_symlink_swap(&vetted).is_ok());
        assert!(w.revalidate_before_write(&vetted).is_ok());
    }

    #[test]
    fn assert_no_symlink_swap_passes_for_a_real_existing_file() {
        let (d, w) = ws();
        std::fs::write(d.path().join("a.compact"), "x").unwrap();
        let vetted = w.resolve("a.compact").unwrap();
        assert!(assert_no_symlink_swap(&vetted).is_ok());
        assert!(w.revalidate_before_write(&vetted).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn revalidate_rejects_a_target_swapped_for_a_symlink_after_resolve() {
        // The core TOCTOU: `resolve("out")` returns an in-root path while `out`
        // does not exist; an attacker then plants `out` -> <outside>/loot before
        // the write. The re-check must refuse.
        let (d, w) = ws();
        let vetted = w.resolve("out").unwrap();

        let outside = tempfile::tempdir().unwrap();
        let loot = outside.path().join("loot");
        std::fs::create_dir(&loot).unwrap();
        std::os::unix::fs::symlink(&loot, d.path().join("out")).unwrap();

        assert!(matches!(
            assert_no_symlink_swap(&vetted),
            Err(CoreError::PathEscape(_))
        ));
        assert!(matches!(
            w.revalidate_before_write(&vetted),
            Err(CoreError::PathEscape(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn revalidate_rejects_a_dangling_symlink_target() {
        // The subtle case the old lexical tail check missed: the planted symlink
        // points at a path that does not exist yet, so `canonicalize` fails on
        // it exactly as it would for an absent tail component. An `lstat` still
        // sees it as a link, so it is refused.
        let (d, w) = ws();
        let vetted = w.resolve("out").unwrap();

        let outside = tempfile::tempdir().unwrap();
        // Target does NOT exist -> a dangling symlink.
        std::os::unix::fs::symlink(outside.path().join("nope"), d.path().join("out")).unwrap();

        assert!(matches!(
            assert_no_symlink_swap(&vetted),
            Err(CoreError::PathEscape(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn revalidate_rejects_a_symlinked_ancestor() {
        // The swap is on an intermediate directory, not the leaf: `a` becomes a
        // symlink out of the root after `resolve("a/b/out")`.
        let (d, w) = ws();
        let vetted = w.resolve("a/b/out").unwrap();

        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), d.path().join("a")).unwrap();

        assert!(matches!(
            assert_no_symlink_swap(&vetted),
            Err(CoreError::PathEscape(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_rejects_a_dangling_symlink_in_the_tail() {
        // Tighten `resolve` itself: a dangling symlink present AT resolve time
        // must not be waved through as a plain non-existent component. `link`
        // points outside the root at a target that does not exist, so
        // `canonicalize` cannot resolve it; the tail check must catch it instead
        // of returning an in-root-looking `<root>/link/x`.
        let (d, w) = ws();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path().join("gone"), d.path().join("link")).unwrap();
        assert!(matches!(w.resolve("link/x"), Err(CoreError::PathEscape(_))));
    }

    #[cfg(unix)]
    #[test]
    fn write_file_refuses_to_follow_a_preplanted_symlink() {
        // The one write WE perform: `O_NOFOLLOW` must stop a symlink planted at
        // the temp file name from redirecting the write outside the root.
        let (_d, w) = ws();
        let scope = w.temp_scope("probe").unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("victim");
        std::os::unix::fs::symlink(&victim, scope.path().join("evil")).unwrap();

        let err = scope.write_file("evil", "pwned").unwrap_err();
        assert!(matches!(err, CoreError::Io(_)), "got {err:?}");
        assert!(
            !victim.exists(),
            "O_NOFOLLOW must not create the symlink target"
        );
    }

    #[cfg(unix)]
    #[test]
    fn method_belt_catches_ancestor_redirect_to_existing_outside_subpath() {
        // The exotic case the root-free guard alone waves through: an ancestor
        // dir is swapped for a symlink out of the root AND the redirected
        // subpath already exists as a real dir. `assert_no_symlink_swap` sees
        // only real components and passes; the method's root-aware belt
        // canonicalizes and rejects. This documents exactly why call sites with a
        // `Workspace` should prefer the method.
        let (d, w) = ws();
        let vetted = w.resolve("a/b/out").unwrap();

        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(outside.path().join("b")).unwrap();
        std::os::unix::fs::symlink(outside.path(), d.path().join("a")).unwrap();

        // Root-free guard: `<root>/a/b` exists through the link as a real dir.
        assert!(
            assert_no_symlink_swap(&vetted).is_ok(),
            "root-free guard cannot see this redirect — this is its documented residual"
        );
        // Method belt: canonicalizes `<root>/a/b` -> `<outside>/b`, outside root.
        assert!(matches!(
            w.revalidate_before_write(&vetted),
            Err(CoreError::PathEscape(_))
        ));
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
