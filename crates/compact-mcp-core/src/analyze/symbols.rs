use std::collections::HashSet;

use compactp_ast::{AstNode, Item, SourceFile, SyntaxNode};
use serde::Serialize;
use serde_json::json;

use super::parse_root;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SymbolKind {
    Ledger,
    Circuit,
    CircuitDecl,
    Witness,
    Contract,
    Struct,
    Enum,
    Module,
    TypeDecl,
    Constructor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Symbol {
    pub kind: SymbolKind,
    pub name: String,
    pub exported: bool,
    /// `"sealed"`, `"pure"`, or absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The enclosing module's qualified path (e.g. `"Outer.Inner"`) when this
    /// declaration is nested inside one or more `module { … }` blocks; absent for
    /// top-level declarations. `name` remains the bare declaration name so callers
    /// can still match it against export lists and lookups.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
}

fn tok(t: Option<compactp_ast::SyntaxToken>) -> String {
    t.map(|t| t.text().to_string()).unwrap_or_default()
}

pub fn symbols(source: &str) -> Vec<Symbol> {
    let (root, _) = parse_root(source);
    let Some(file) = SourceFile::cast(root) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    collect_scope(file.syntax(), None, &mut out);
    out
}

/// Emit a [`Symbol`] for every declaration directly inside `scope`, recursing into
/// nested `module { … }` bodies (module declarations live as direct children of the
/// `MODULE_DEF` node — the same shape `SourceFile::items()` walks at the top level).
///
/// Output is depth-first in source order: a module is emitted immediately before its
/// members. `module` carries the enclosing module's qualified path, or `None` at the
/// top level.
///
/// The `export { … }` list form is scoped: names it lists mark only the declarations
/// in the *same* brace scope (top-level lists cannot reach into a module and vice
/// versa), matching Compact's own scoping.
///
/// Recursion depth is bounded by module nesting, which is bounded in turn by
/// `MAX_STRUCTURAL_DEPTH` (every `module { }` opens a brace the depth guard counts),
/// so this cannot overflow the stack on any input `parse_root` accepts.
fn collect_scope(scope: &SyntaxNode, module: Option<&str>, out: &mut Vec<Symbol>) {
    // The qualified path stamped onto every symbol emitted directly in this scope.
    let owned_module = || module.map(str::to_string);

    // Names collected from any `export { … }` list(s) in this scope.
    let mut export_names: HashSet<String> = HashSet::new();
    // Indices into `out` of the symbols emitted *directly* in this scope (not those a
    // recursive call pushed for a nested module), so this scope's export list marks
    // only this scope's declarations.
    let mut local: Vec<usize> = Vec::new();

    for item in scope.children().filter_map(Item::cast) {
        let symbol = match item {
            Item::LedgerDecl(d) => Symbol {
                kind: SymbolKind::Ledger,
                name: tok(d.name()),
                exported: d.is_exported(),
                detail: d.is_sealed().then(|| "sealed".to_string()),
                module: owned_module(),
            },
            Item::CircuitDef(d) => Symbol {
                kind: SymbolKind::Circuit,
                name: tok(d.name()),
                exported: d.is_exported(),
                detail: d.is_pure().then(|| "pure".to_string()),
                module: owned_module(),
            },
            Item::CircuitDecl(d) => Symbol {
                kind: SymbolKind::CircuitDecl,
                name: tok(d.name()),
                exported: d.is_exported(),
                detail: None,
                module: owned_module(),
            },
            Item::WitnessDecl(d) => Symbol {
                kind: SymbolKind::Witness,
                name: tok(d.name()),
                exported: d.is_exported(),
                detail: None,
                module: owned_module(),
            },
            Item::ContractDecl(d) => Symbol {
                kind: SymbolKind::Contract,
                name: tok(d.name()),
                exported: d.is_exported(),
                detail: None,
                module: owned_module(),
            },
            Item::StructDef(d) => Symbol {
                kind: SymbolKind::Struct,
                name: tok(d.name()),
                exported: d.is_exported(),
                detail: None,
                module: owned_module(),
            },
            Item::EnumDef(d) => Symbol {
                kind: SymbolKind::Enum,
                name: tok(d.name()),
                exported: d.is_exported(),
                detail: None,
                module: owned_module(),
            },
            Item::ModuleDef(d) => {
                let name = tok(d.name());
                // Emit the module itself into this scope, then descend. It is pushed
                // before its members (depth-first) and is itself a member of this
                // scope (so an `export { Foo }` in this scope can mark it).
                local.push(out.len());
                out.push(Symbol {
                    kind: SymbolKind::Module,
                    name: name.clone(),
                    exported: d.is_exported(),
                    detail: None,
                    module: owned_module(),
                });
                let child = match module {
                    Some(parent) => format!("{parent}.{name}"),
                    None => name,
                };
                collect_scope(d.syntax(), Some(&child), out);
                continue;
            }
            Item::TypeDecl(d) => Symbol {
                kind: SymbolKind::TypeDecl,
                name: tok(d.name()),
                exported: d.is_exported(),
                detail: None,
                module: owned_module(),
            },
            Item::ConstructorDef(_) => Symbol {
                kind: SymbolKind::Constructor,
                name: "constructor".to_string(),
                exported: false,
                detail: None,
                module: owned_module(),
            },
            Item::ExportList(list) => {
                // The list form exports by reference: record the names and mark the
                // matching declarations below. Not a symbol in its own right.
                export_names.extend(list.names().map(|t| t.text().to_string()));
                continue;
            }
            // Pragma / Include / Import are not symbols.
            _ => continue,
        };
        local.push(out.len());
        out.push(symbol);
    }

    // Apply this scope's `export { … }` list(s) to this scope's declarations.
    if !export_names.is_empty() {
        for &idx in &local {
            if export_names.contains(&out[idx].name) {
                out[idx].exported = true;
            }
        }
    }
}

/// A compact JSON projection of the typed AST: the item list, tagged by kind.
/// We do not dump the CST — it would flood an agent's context window.
pub fn ast_json(source: &str) -> serde_json::Value {
    let items: Vec<serde_json::Value> = symbols(source)
        .into_iter()
        .map(|s| {
            let mut item = json!({
                "kind": s.kind,
                "name": s.name,
                "exported": s.exported,
                "detail": s.detail,
            });
            // Only module-nested declarations carry a `module`; top-level items keep
            // the original shape (no key) so existing consumers are unaffected.
            if let Some(m) = s.module {
                item["module"] = json!(m);
            }
            item
        })
        .collect();
    json!({ "kind": "SourceFile", "items": items })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"
pragma language_version >= 0.23;
import CompactStandardLibrary;
export ledger round: Counter;
sealed ledger owner: Bytes<32>;
witness secret_value(): Field;
export pure circuit double(x: Field): Field { return x + x; }
export circuit increment(): [] { round.increment(1); }
struct Point { x: Field, y: Field }
enum Colour { Red, Green }
"#;

    fn find<'a>(v: &'a [Symbol], name: &str) -> &'a Symbol {
        v.iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no symbol {name}"))
    }

    #[test]
    fn indexes_every_declaration_kind() {
        let s = symbols(SRC);
        assert_eq!(find(&s, "round").kind, SymbolKind::Ledger);
        assert_eq!(find(&s, "owner").kind, SymbolKind::Ledger);
        assert_eq!(find(&s, "secret_value").kind, SymbolKind::Witness);
        assert_eq!(find(&s, "double").kind, SymbolKind::Circuit);
        assert_eq!(find(&s, "increment").kind, SymbolKind::Circuit);
        assert_eq!(find(&s, "Point").kind, SymbolKind::Struct);
        assert_eq!(find(&s, "Colour").kind, SymbolKind::Enum);
    }

    #[test]
    fn records_exported_and_sealed_and_pure() {
        let s = symbols(SRC);
        assert!(find(&s, "round").exported);
        assert!(!find(&s, "secret_value").exported);
        assert_eq!(find(&s, "owner").detail.as_deref(), Some("sealed"));
        assert_eq!(find(&s, "double").detail.as_deref(), Some("pure"));
        assert_eq!(find(&s, "increment").detail, None);
    }

    #[test]
    fn ast_json_has_a_source_file_root_with_items() {
        let v = ast_json(SRC);
        assert_eq!(v["kind"], "SourceFile");
        assert!(v["items"].as_array().unwrap().len() >= 7);
    }

    #[test]
    fn top_level_declarations_have_no_module() {
        // The nesting field is opt-in: every symbol in a module-free source omits it.
        assert!(symbols(SRC).iter().all(|s| s.module.is_none()));
    }

    /// Regression for the omission where `symbols()` walked only top-level items and
    /// listed a module by name while dropping every declaration inside it.
    #[test]
    fn module_nested_declarations_are_listed() {
        const NESTED: &str = r#"
pragma language_version >= 0.23;
export ledger round: Counter;
module Vault {
  witness secret(): Field;
  export circuit run(): [] {}
}
"#;
        let s = symbols(NESTED);

        // Before the fix this was 2 ([round, Vault]); the two module members vanished.
        assert_eq!(s.len(), 4, "expected round, Vault, secret, run");

        // Top-level items keep `module: None`.
        assert_eq!(find(&s, "round").module, None);
        assert_eq!(find(&s, "Vault").module, None);
        assert_eq!(find(&s, "Vault").kind, SymbolKind::Module);

        // Nested members are now emitted, tagged with their enclosing module, and keep
        // their bare name.
        assert_eq!(find(&s, "secret").kind, SymbolKind::Witness);
        assert_eq!(find(&s, "secret").module.as_deref(), Some("Vault"));
        assert_eq!(find(&s, "run").kind, SymbolKind::Circuit);
        assert_eq!(find(&s, "run").module.as_deref(), Some("Vault"));

        // Depth-first order: a module is emitted immediately before its members.
        let names: Vec<&str> = s.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["round", "Vault", "secret", "run"]);
    }

    /// A declaration exported inside a nested module reports `exported: true`, and the
    /// module path is carried for arbitrarily deep nesting.
    #[test]
    fn nested_module_paths_are_qualified() {
        const DEEP: &str = r#"
pragma language_version >= 0.23;
module Outer {
  module Inner {
    export circuit deep(): [] {}
  }
}
"#;
        let s = symbols(DEEP);
        assert_eq!(find(&s, "Outer").module, None);
        assert_eq!(find(&s, "Inner").module.as_deref(), Some("Outer"));
        assert_eq!(find(&s, "deep").module.as_deref(), Some("Outer.Inner"));
    }

    /// Regression for the omission where `export { … }` was skipped, leaving a
    /// list-exported declaration reported as private.
    #[test]
    fn export_list_marks_declarations_exported() {
        const LIST: &str = r#"
pragma language_version >= 0.23;
witness secret(): Field;
circuit run(): [] {}
export { secret, run };
"#;
        let s = symbols(LIST);
        // Before the fix both were `exported: false`.
        assert!(find(&s, "secret").exported, "export {{ }} list ignored");
        assert!(find(&s, "run").exported, "export {{ }} list ignored");
        // The list itself is not emitted as a symbol.
        assert_eq!(s.len(), 2);
    }

    /// Export lists are scoped: a top-level `export { name }` must not reach a
    /// same-named declaration nested inside a module, and vice versa.
    #[test]
    fn export_lists_are_scoped_to_their_brace_level() {
        const SCOPED: &str = r#"
pragma language_version >= 0.23;
witness shared(): Field;
module M {
  witness shared(): Field;
  export { shared };
}
"#;
        let s = symbols(SCOPED);
        // The module-level `export { shared }` marks only the module's `shared`.
        let top = s
            .iter()
            .find(|s| s.name == "shared" && s.module.is_none())
            .expect("top-level shared");
        let nested = s
            .iter()
            .find(|s| s.name == "shared" && s.module.as_deref() == Some("M"))
            .expect("module-level shared");
        assert!(
            !top.exported,
            "top-level decl wrongly exported by module list"
        );
        assert!(nested.exported, "module list did not export module decl");
    }
}
