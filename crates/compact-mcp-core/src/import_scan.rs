//! Reject `import` / `include` targets that escape the workspace root.
//!
//! The workspace gate ([`Workspace::resolve`]) validates the ENTRY source path a
//! client supplies, but the compiler independently dereferences the paths inside
//! `import "…"` and `include "…"` directives. Empirically (compiler 0.31.1 via
//! `compact` 0.5.1) it resolves those targets *relative to the directory of the
//! including/importing file*, following `../` traversal and absolute paths
//! **outside the workspace root**, and it appends `.compact` to the target
//! unconditionally. That yields a filesystem existence oracle and a content leak
//! of out-of-root `*.compact` files through the diagnostics the compiler returns
//! (e.g. `include "../secret"` surfaces a parse-error snippet of `../secret.compact`).
//!
//! No compiler flag constrains the search root: `--compact-path` only *adds*
//! directories (the compiler always looks relative to the including file first),
//! and `--sourceRoot` is a cosmetic source-map field. So we close the hole
//! in-process. Before invoking the compiler we parse the entry source — and every
//! in-root `.compact` file it *transitively* pulls in — with the same `compactp`
//! front end `analyze` uses, extract every `import`/`include` string-literal
//! target, and reject any that resolves outside the root, reusing
//! [`Workspace::resolve`]'s containment semantics (canonicalization, symlink
//! resolution, lexical `..`).
//!
//! ## Guarantee and its edges
//!
//! * **Direct targets are gated unconditionally.** Every `import`/`include` path
//!   literal in the entry source is checked before the compiler runs.
//! * **Transitive targets are gated** by recursively scanning the in-root files
//!   the compiler would read next. The recursion is fail-closed: an over-deep,
//!   oversize, or unreadable in-root file it reaches is a rejection, not a skip.
//! * Identifier-form imports (`import CompactStandardLibrary;`, `import Foo;`)
//!   cannot express a path and are ignored. A path literal containing a backslash
//!   escape is refused rather than guessed at (real Compact paths have none), so
//!   we never clear a literal we cannot interpret exactly as the compiler would.
//! * Directories on an operator-configured `COMPACT_PATH` are out of scope: they
//!   are trusted configuration, not attacker-controlled source content.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use compactp_ast::{AstNode, Import, Include};

use crate::{CoreError, Workspace, analyze};

/// Upper bound on the number of files the transitive scan will read. A real
/// contract's include graph is tiny; this only bounds a pathological or
/// adversarial fan-out, and reaching it is a (fail-closed) rejection.
const MAX_SCANNED_FILES: usize = 256;

/// Reject any `import`/`include` target in `entry_source` — or in any in-root
/// `.compact` file it transitively includes — that resolves outside `ws`'s root.
///
/// `entry_path` is the already-resolved (in-root) path of the file whose text is
/// `entry_source`; targets are resolved relative to the directory of the file
/// that contains them, exactly as the compiler resolves them.
pub fn assert_imports_contained(
    ws: &Workspace,
    entry_path: &Path,
    entry_source: &str,
) -> Result<(), CoreError> {
    let mut visited: HashSet<PathBuf> = HashSet::new();
    visited.insert(entry_path.to_path_buf());
    // The entry counts as one scanned file; children draw down the remainder.
    let mut budget = MAX_SCANNED_FILES.saturating_sub(1);
    scan(ws, entry_path, entry_source, &mut visited, &mut budget)
}

fn scan(
    ws: &Workspace,
    file: &Path,
    source: &str,
    visited: &mut HashSet<PathBuf>,
    budget: &mut usize,
) -> Result<(), CoreError> {
    // The directory the compiler resolves this file's non-absolute targets
    // against. A resolved in-root file always has a parent inside the root.
    let dir = file.parent().unwrap_or_else(|| ws.root());

    for target in directive_targets(source)? {
        // The compiler opens `<target>.compact` (it appends the extension
        // unconditionally). `dir.join` yields an absolute path (dir is absolute),
        // and an absolute `target` replaces it — so `resolve` sees the real path.
        // Check BOTH the bare target (catches every `../`/absolute *directory*
        // escape) AND the `.compact` file the compiler actually opens (which also
        // catches a symlink AT the leaf, e.g. `link.compact -> /outside`).
        let bare = dir.join(&target);
        contain(ws, &bare, &target)?;

        let child = contain(ws, &append_compact(&bare), &target)?;

        // Recurse into an in-root file the compiler would actually read next, so
        // a transitive `include` chain cannot smuggle the escape one hop away.
        // `child` is already canonical and in-root (contain() canonicalizes).
        if child.is_file() && visited.insert(child.clone()) {
            *budget = budget.checked_sub(1).ok_or_else(|| {
                CoreError::InvalidArgs(format!(
                    "include/import graph exceeds {MAX_SCANNED_FILES} files; refusing to scan"
                ))
            })?;
            let text = read_capped(&child)?;
            scan(ws, &child, &text, visited, budget)?;
        }
    }
    Ok(())
}

/// Every `import "…"` / `include "…"` string-literal target in `source`,
/// including those nested inside `module { … }` blocks (the compiler follows
/// those too). Identifier-form imports carry no path and are skipped.
///
/// Fails closed on structurally over-deep input (consistent with `analyze`,
/// whose depth guard would otherwise hand back an empty tree and hide any import
/// in it) and on a path literal using an escape we will not interpret.
fn directive_targets(source: &str) -> Result<Vec<String>, CoreError> {
    if analyze::is_over_max_depth(source) {
        return Err(CoreError::InvalidArgs(analyze::depth_refusal_message()));
    }
    let (root, _diags) = analyze::parse_root(source);

    let mut out = Vec::new();
    for node in root.descendants() {
        let path_tok = if let Some(inc) = Include::cast(node.clone()) {
            inc.path()
        } else if let Some(imp) = Import::cast(node.clone()) {
            // `Import::path()` is the string-literal form (`import "…"`,
            // `import { a } from "…"`); the identifier form has none.
            imp.path()
        } else {
            None
        };
        let Some(tok) = path_tok else { continue };
        match unquote(tok.text()) {
            Some(p) => out.push(p),
            None => {
                return Err(CoreError::InvalidArgs(format!(
                    "import/include path {:?} uses an unsupported escape; refusing to \
                     compile (it may escape the workspace root)",
                    tok.text()
                )));
            }
        }
    }
    Ok(out)
}

/// Resolve `candidate` through the workspace gate, translating a containment
/// rejection into a clear, import-specific error that names the offending
/// directive target (the client's own literal, never a canonicalized external
/// path). The `"escapes workspace root"` wording matches the boundary the rest of
/// the crate cites.
fn contain(ws: &Workspace, candidate: &Path, target: &str) -> Result<PathBuf, CoreError> {
    ws.resolve(candidate).map_err(|_| {
        CoreError::InvalidArgs(format!(
            "import/include target {target:?} escapes workspace root"
        ))
    })
}

/// Append `.compact` to `p` (the compiler adds it unconditionally, so
/// `foo.compact` becomes `foo.compact.compact`). Distinct from
/// [`Path::with_extension`], which would *replace* an existing extension.
fn append_compact(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(".compact");
    PathBuf::from(s)
}

/// Read an in-root file we intend to keep scanning, capped at
/// [`crate::MAX_SOURCE_BYTES`]. Fail-closed: an oversize or unreadable file the
/// compiler *would* read is a rejection, not a silent gap in the transitive scan.
fn read_capped(path: &Path) -> Result<String, CoreError> {
    let len = std::fs::metadata(path)?.len() as usize;
    if len > crate::MAX_SOURCE_BYTES {
        return Err(CoreError::InvalidArgs(format!(
            "included file too large to scan for imports: {} bytes (max {}) at {}",
            len,
            crate::MAX_SOURCE_BYTES,
            path.display()
        )));
    }
    Ok(std::fs::read_to_string(path)?)
}

/// Strip the surrounding quotes from a `STRING_LIT` lexeme, returning its literal
/// path text. Returns `None` — meaning "do not interpret" — for a lexeme that is
/// not a plainly-quoted string or that contains a backslash escape: real Compact
/// import paths contain no escapes, and refusing to guess one keeps us from
/// clearing a literal that could resolve to a *different* (out-of-root) path than
/// the compiler would open. Callers treat `None` as a rejection, never a skip.
fn unquote(lexeme: &str) -> Option<String> {
    let bytes = lexeme.as_bytes();
    if bytes.len() < 2 {
        return None;
    }
    let quote = bytes[0];
    if (quote != b'"' && quote != b'\'') || bytes[bytes.len() - 1] != quote {
        return None;
    }
    let inner = &lexeme[1..lexeme.len() - 1];
    if inner.contains('\\') {
        return None;
    }
    Some(inner.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace root plus an out-of-root secret sibling, mirroring the real
    /// leak topology: `base/secret.compact` (with a unique marker) lives OUTSIDE
    /// `base/root` (the workspace).
    struct Fixture {
        _base: tempfile::TempDir,
        ws: Workspace,
        root: PathBuf,
    }

    const MARKER: &str = "OUT_OF_ROOT_MARKER_9f3a";

    impl Fixture {
        fn new() -> Self {
            let base = tempfile::tempdir().unwrap();
            let root = base.path().join("root");
            std::fs::create_dir(&root).unwrap();
            std::fs::write(
                base.path().join("secret.compact"),
                format!("pragma language_version >= 0.23;\nledger {MARKER}: Counter;\n"),
            )
            .unwrap();
            let ws = Workspace::new(&root).unwrap();
            Self {
                _base: base,
                ws,
                root,
            }
        }

        /// Write `source` to `<root>/<name>` and return its resolved path.
        fn write(&self, name: &str, source: &str) -> PathBuf {
            let p = self.root.join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, source).unwrap();
            self.ws.resolve(name).unwrap()
        }

        fn check(&self, entry: &Path, source: &str) -> Result<(), CoreError> {
            assert_imports_contained(&self.ws, entry, source)
        }
    }

    fn escapes(r: &Result<(), CoreError>) -> bool {
        matches!(r, Err(CoreError::InvalidArgs(m)) if m.contains("escapes workspace root"))
    }

    #[test]
    fn unquote_strips_quotes_and_refuses_escapes() {
        assert_eq!(unquote(r#""../a/b""#).as_deref(), Some("../a/b"));
        assert_eq!(unquote("'../a/b'").as_deref(), Some("../a/b"));
        assert_eq!(unquote(r#""""#).as_deref(), Some(""));
        // Backslash escape: refuse (returns None -> caller rejects).
        assert_eq!(unquote(r#""a\nb""#), None);
        // Not a quoted string.
        assert_eq!(unquote("bare"), None);
        assert_eq!(unquote(r#"""#), None);
    }

    #[test]
    fn directive_targets_extracts_include_and_import_paths() {
        let src = r#"
pragma language_version >= 0.23;
import CompactStandardLibrary;
include "./local";
include "../escape";
import "utils/Auth";
import { thing } from "../mod";
import Named;
"#;
        let got = directive_targets(src).unwrap();
        // Named / stdlib imports carry no path; only the four quoted ones appear.
        assert_eq!(got, vec!["./local", "../escape", "utils/Auth", "../mod"]);
    }

    #[test]
    fn directive_targets_finds_module_nested_directives() {
        // The compiler follows an include INSIDE a module block; the scan must see
        // it too (walking all descendants, not just top-level items).
        let src = r#"
pragma language_version >= 0.23;
module Inner {
  include "../secret";
}
"#;
        let got = directive_targets(src).unwrap();
        assert_eq!(got, vec!["../secret"]);
    }

    #[test]
    fn in_root_relative_import_is_allowed() {
        let f = Fixture::new();
        // A sibling in-root module the entry legitimately imports.
        f.write("Auth.compact", "pragma language_version >= 0.23;\n");
        let src = "pragma language_version >= 0.23;\ninclude \"Auth\";\n";
        let entry = f.write("main.compact", src);
        assert!(f.check(&entry, src).is_ok());
    }

    #[test]
    fn dotdot_include_escaping_root_is_rejected() {
        let f = Fixture::new();
        // `include "../secret"` -> compiler opens `<root>/../secret.compact`.
        let src = "pragma language_version >= 0.23;\ninclude \"../secret\";\n";
        let entry = f.write("main.compact", src);
        assert!(escapes(&f.check(&entry, src)));
    }

    #[test]
    fn absolute_path_import_is_rejected() {
        // The acceptance-criterion vector: `import "../../etc/hostname"`-style
        // traversal AND a bare absolute path both escape.
        let f = Fixture::new();
        for directive in [
            "import \"/etc/hostname\";",
            "include \"/etc/hostname\";",
            "import \"../../../../../../etc/hostname\";",
        ] {
            let src = format!("pragma language_version >= 0.23;\n{directive}\n");
            let entry = f.write("main.compact", &src);
            assert!(escapes(&f.check(&entry, &src)), "not rejected: {directive}");
        }
    }

    #[test]
    fn transitive_include_escaping_root_is_rejected() {
        // Entry (in-root) includes an in-root helper that itself escapes. The
        // direct scan of the entry sees only the in-root helper; the recursion is
        // what catches the escape one hop away.
        let f = Fixture::new();
        f.write(
            "helper.compact",
            "pragma language_version >= 0.23;\ninclude \"../secret\";\n",
        );
        let src = "pragma language_version >= 0.23;\ninclude \"helper\";\n";
        let entry = f.write("main.compact", src);
        assert!(escapes(&f.check(&entry, src)));
    }

    #[test]
    fn inline_source_from_a_nested_temp_dir_escapes_via_dotdot() {
        // Mirrors the inline-`source` case: the entry lives a few levels under the
        // root (as the temp scope does), and a `../…` chain climbs out. From
        // `<root>/a/b`, three `..` reach `<base>` (the parent of the root).
        let f = Fixture::new();
        let src = "pragma language_version >= 0.23;\ninclude \"../../../secret\";\n";
        let entry = f.write("a/b/input.compact", src);
        assert!(escapes(&f.check(&entry, src)));
    }

    #[test]
    fn escaped_path_literal_is_refused_not_guessed() {
        let f = Fixture::new();
        let src = "pragma language_version >= 0.23;\ninclude \"..\\\\secret\";\n";
        let entry = f.write("main.compact", src);
        let err = f.check(&entry, src).unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgs(m) if m.contains("unsupported escape")));
    }

    #[test]
    fn a_cycle_between_in_root_files_terminates() {
        // Two in-root files that include each other must not loop forever.
        let f = Fixture::new();
        f.write(
            "a.compact",
            "pragma language_version >= 0.23;\ninclude \"b\";\n",
        );
        f.write(
            "b.compact",
            "pragma language_version >= 0.23;\ninclude \"a\";\n",
        );
        let src = "pragma language_version >= 0.23;\ninclude \"a\";\n";
        let entry = f.write("main.compact", src);
        assert!(f.check(&entry, src).is_ok());
    }

    #[test]
    fn clean_contract_with_no_path_imports_is_allowed() {
        let f = Fixture::new();
        let src = "pragma language_version >= 0.23;\nimport CompactStandardLibrary;\n\
                   export ledger n: Counter;\nexport circuit c(): [] { n.increment(1); }\n";
        let entry = f.write("main.compact", src);
        assert!(f.check(&entry, src).is_ok());
    }
}
