use compactp_ast::{AstNode, Item, SourceFile};
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
    for item in file.items() {
        let s = match item {
            Item::LedgerDecl(d) => Symbol {
                kind: SymbolKind::Ledger,
                name: tok(d.name()),
                exported: d.is_exported(),
                detail: d.is_sealed().then(|| "sealed".to_string()),
            },
            Item::CircuitDef(d) => Symbol {
                kind: SymbolKind::Circuit,
                name: tok(d.name()),
                exported: d.is_exported(),
                detail: d.is_pure().then(|| "pure".to_string()),
            },
            Item::CircuitDecl(d) => Symbol {
                kind: SymbolKind::CircuitDecl,
                name: tok(d.name()),
                exported: d.is_exported(),
                detail: None,
            },
            Item::WitnessDecl(d) => Symbol {
                kind: SymbolKind::Witness,
                name: tok(d.name()),
                exported: d.is_exported(),
                detail: None,
            },
            Item::ContractDecl(d) => Symbol {
                kind: SymbolKind::Contract,
                name: tok(d.name()),
                exported: d.is_exported(),
                detail: None,
            },
            Item::StructDef(d) => Symbol {
                kind: SymbolKind::Struct,
                name: tok(d.name()),
                exported: d.is_exported(),
                detail: None,
            },
            Item::EnumDef(d) => Symbol {
                kind: SymbolKind::Enum,
                name: tok(d.name()),
                exported: d.is_exported(),
                detail: None,
            },
            Item::ModuleDef(d) => Symbol {
                kind: SymbolKind::Module,
                name: tok(d.name()),
                exported: d.is_exported(),
                detail: None,
            },
            Item::TypeDecl(d) => Symbol {
                kind: SymbolKind::TypeDecl,
                name: tok(d.name()),
                exported: d.is_exported(),
                detail: None,
            },
            Item::ConstructorDef(_) => Symbol {
                kind: SymbolKind::Constructor,
                name: "constructor".to_string(),
                exported: false,
                detail: None,
            },
            // Pragma / Include / Import / ExportList are not symbols.
            _ => continue,
        };
        out.push(s);
    }
    out
}

/// A compact JSON projection of the typed AST: the item list, tagged by kind.
/// We do not dump the CST — it would flood an agent's context window.
pub fn ast_json(source: &str) -> serde_json::Value {
    let items: Vec<serde_json::Value> = symbols(source)
        .into_iter()
        .map(|s| {
            json!({
                "kind": s.kind,
                "name": s.name,
                "exported": s.exported,
                "detail": s.detail,
            })
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
}
