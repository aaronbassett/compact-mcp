use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::CoreError;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TypeRef {
    #[serde(rename = "type-name")]
    pub type_name: String,
    /// `Bytes<N>` carries `length` (byte width). `Vector<N, T>` ALSO emits a
    /// `length` key here — the element COUNT, a different meaning — alongside a
    /// singular `type` key (see [`TypeRef::element_type`]) for the element type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,
    /// `Uint<N>` carries `maxval` — the maximum value, NOT the bit width.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maxval: Option<u64>,
    /// `Struct` and `Enum` carry a `name` (the declared type name, e.g. `Point`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A `Vector<N, T>`'s element type: the compiler emits it under a SINGULAR
    /// `type` key, distinct from the PLURAL `types` array a `Tuple` emits. Boxed
    /// to break the otherwise-infinite recursive type size.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub element_type: Option<Box<TypeRef>>,
    /// A `Tuple`'s member types — a PLURAL `types` array.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<TypeRef>,
    /// The compiler spells both a `Struct`'s fields and an `Enum`'s variants with
    /// the same `elements` key, but with different member shapes: a struct field
    /// is a `{ "name", "type" }` object, an enum variant is a bare string. See
    /// [`TypeElement`], which models that union.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub elements: Vec<TypeElement>,
}

/// A member of a composite type's `elements` array. Untagged because the
/// compiler reuses one `elements` key for two shapes — a `Struct` field is a
/// `{ "name", "type" }` object; an `Enum` variant is a bare string like
/// `"red"` — so each member deserializes to whichever shape it matches.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum TypeElement {
    /// A `Struct` field: `{ "name": "x", "type": { "type-name": "Field" } }`.
    Field(StructField),
    /// An `Enum` variant name: a bare string such as `"red"`.
    Variant(String),
}

/// A single named field of a `Struct` type, as it appears in `elements`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StructField {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: TypeRef,
}

impl TypeRef {
    pub fn named(n: &str) -> Self {
        Self {
            type_name: n.to_string(),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Argument {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: TypeRef,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Circuit {
    pub name: String,
    pub pure: bool,
    /// A `.zkir` file exists for this circuit **iff** `proof` is true.
    pub proof: bool,
    #[serde(default)]
    pub arguments: Vec<Argument>,
    /// Circuits spell this key with a HYPHEN and witnesses with a SPACE. Accept
    /// both on each struct so an upstream normalisation to one spelling does not
    /// silently yield a default `TypeRef`. A missing key (either spelling) still
    /// fails loudly, because this field has no `#[serde(default)]`.
    #[serde(rename = "result-type", alias = "result type")]
    pub result_type: TypeRef,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Witness {
    pub name: String,
    #[serde(default)]
    pub arguments: Vec<Argument>,
    /// Upstream spells this key with a SPACE for witnesses, and with a HYPHEN
    /// for circuits. Accept both so a rename upstream does not silently yield
    /// a default value.
    #[serde(rename = "result type", alias = "result-type")]
    pub result_type: TypeRef,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LedgerField {
    pub name: String,
    pub index: u64,
    pub exported: bool,
    pub storage: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContractInfo {
    #[serde(rename = "compiler-version")]
    pub compiler_version: String,
    #[serde(rename = "language-version")]
    pub language_version: String,
    #[serde(rename = "runtime-version")]
    pub runtime_version: String,
    /// No `#[serde(default)]` — deliberately. The compiler ALWAYS emits these four
    /// structural keys (as `[]` when empty), so a MISSING key means the schema
    /// drifted (a rename or omission upstream), not "an empty contract". Defaulting
    /// a missing `circuits` to `[]` would let a drifted artifact read as
    /// circuit-less rather than failing loudly — the same fail-loud stance `zkir`
    /// takes for its instruction list. An absent key is therefore a hard
    /// deserialization error.
    pub circuits: Vec<Circuit>,
    /// Only witnesses that some circuit actually CALLS appear here. A declared
    /// but unreferenced witness is absent — compare with the `symbols` tool.
    pub witnesses: Vec<Witness>,
    pub contracts: Vec<serde_json::Value>,
    pub ledger: Vec<LedgerField>,
}

impl ContractInfo {
    pub fn load(path: &Path) -> Result<Self, CoreError> {
        // Size-cap the read before it hits memory. `read_to_string_capped`
        // preserves the prior error mapping: only a genuine "not found" is an
        // ArtifactMissing; a permission error, is-a-directory, or non-UTF-8/read
        // failure surfaces as Io so the caller is not misdirected into looking
        // for a file that is actually there.
        let text = super::read_to_string_capped(path)?;
        serde_json::from_str(&text).map_err(|e| CoreError::MalformedArtifact {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })
    }
}

/// Compact type -> TypeScript type, mirroring exactly what the compiler itself
/// emits for witness signatures in `index.d.ts`. All mappings below are verified
/// against real `compact compile --skip-zk` output:
///
/// - `Field` / `Uint<N>` -> `bigint`
/// - `Boolean` -> `boolean`
/// - `Bytes<N>` -> `Uint8Array`
/// - `Tuple` -> `[]` (empty) or `[A, B]` (its plural `types`)
/// - `Vector<N, T>` -> `T[]` (its singular `type` element, recursively mapped)
/// - `Struct { x: A, y: B }` -> `{ x: A, y: B }` (structural literal from
///   `elements`; an empty struct yields `{  }`, exactly as the compiler emits)
/// - `Enum` -> `number` (the compiler represents each variant by its ordinal)
///
/// [`unknown`] is reserved for genuinely unrepresentable types (and warns), so a
/// recoverable type is never silently downgraded.
pub fn ts_type(t: &TypeRef) -> String {
    match t.type_name.as_str() {
        "Field" | "Uint" => "bigint".to_string(),
        "Boolean" => "boolean".to_string(),
        "Bytes" => "Uint8Array".to_string(),
        "Tuple" if t.types.is_empty() => "[]".to_string(),
        "Tuple" => format!(
            "[{}]",
            t.types.iter().map(ts_type).collect::<Vec<_>>().join(", ")
        ),
        // The element type is the SINGULAR `type` key. Its absence would mean the
        // compiler stopped emitting it, which is not representable -> `unknown`.
        "Vector" => match &t.element_type {
            Some(el) => format!("{}[]", ts_type(el)),
            None => unknown(t),
        },
        // A structural object literal built from the struct's fields, matching the
        // compiler's own `index.d.ts`. Enum variants (bare strings) never appear
        // under a `Struct`, so filtering to `Field` members loses nothing.
        "Struct" => {
            let fields: Vec<String> = t
                .elements
                .iter()
                .filter_map(|e| match e {
                    TypeElement::Field(f) => Some(format!("{}: {}", f.name, ts_type(&f.ty))),
                    TypeElement::Variant(_) => None,
                })
                .collect();
            format!("{{ {} }}", fields.join(", "))
        }
        "Enum" => "number".to_string(),
        _ => unknown(t),
    }
}

/// The honest fallback for a type this mapper cannot faithfully represent: emit
/// `unknown` (never a plausible-but-wrong concrete type) and warn, so a
/// genuinely unrepresentable type is visible in logs rather than silently lossy.
fn unknown(t: &TypeRef) -> String {
    tracing::warn!(
        type_name = %t.type_name,
        "contract-info.json type has no precise TypeScript mapping; emitting `unknown`"
    );
    "unknown".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim output of `compact compile --skip-zk` on a contract with a used
    /// witness taking `Bytes<32>` and `Uint<16>`. Note the two different spellings
    /// of the result-type key.
    const REAL: &str = r#"{
      "compiler-version": "0.31.1",
      "language-version": "0.23.0",
      "runtime-version": "0.16.0",
      "circuits": [
        { "name": "put", "pure": false, "proof": true,
          "arguments": [{ "name": "k", "type": { "type-name": "Bytes", "length": 32 } }],
          "result-type": { "type-name": "Tuple", "types": [] } }
      ],
      "witnesses": [
        { "name": "lookup",
          "arguments": [
            { "name": "key", "type": { "type-name": "Bytes", "length": 32 } },
            { "name": "idx", "type": { "type-name": "Uint", "maxval": 65535 } }
          ],
          "result type": { "type-name": "Field" } },
        { "name": "flag", "arguments": [], "result type": { "type-name": "Boolean" } }
      ],
      "contracts": [],
      "ledger": [{ "name": "n", "index": 0, "exported": true, "storage": "Counter" }]
    }"#;

    #[test]
    fn deserializes_both_result_type_spellings() {
        let ci: ContractInfo = serde_json::from_str(REAL).unwrap();
        // circuits use "result-type"; witnesses use "result type" (with a space).
        assert_eq!(ci.circuits[0].result_type.type_name, "Tuple");
        assert_eq!(ci.witnesses[0].result_type.type_name, "Field");
        assert_eq!(ci.witnesses[1].result_type.type_name, "Boolean");
    }

    #[test]
    fn captures_bytes_length_and_uint_maxval() {
        let ci: ContractInfo = serde_json::from_str(REAL).unwrap();
        let w = &ci.witnesses[0];
        assert_eq!(w.arguments[0].ty.length, Some(32));
        assert_eq!(w.arguments[1].ty.maxval, Some(65535));
        assert_eq!(w.arguments[1].ty.length, None);
    }

    #[test]
    fn maps_compact_types_to_typescript() {
        assert_eq!(ts_type(&TypeRef::named("Field")), "bigint");
        assert_eq!(ts_type(&TypeRef::named("Boolean")), "boolean");
        assert_eq!(
            ts_type(&TypeRef {
                length: Some(32),
                ..TypeRef::named("Bytes")
            }),
            "Uint8Array"
        );
        assert_eq!(
            ts_type(&TypeRef {
                maxval: Some(65535),
                ..TypeRef::named("Uint")
            }),
            "bigint"
        );
        assert_eq!(ts_type(&TypeRef::named("Tuple")), "[]");
        assert_eq!(ts_type(&TypeRef::named("Wat")), "unknown");
    }

    #[test]
    fn versions_are_read_from_the_build_not_the_live_toolchain() {
        let ci: ContractInfo = serde_json::from_str(REAL).unwrap();
        assert_eq!(ci.compiler_version, "0.31.1");
        assert_eq!(ci.language_version, "0.23.0");
    }

    #[test]
    fn load_reads_a_valid_file_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contract-info.json");
        std::fs::write(&path, REAL).unwrap();
        let ci = ContractInfo::load(&path).unwrap();
        assert_eq!(ci.compiler_version, "0.31.1");
        assert_eq!(ci.circuits[0].name, "put");
    }

    #[test]
    fn load_missing_file_is_artifact_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let err = ContractInfo::load(&path).unwrap_err();
        assert!(
            matches!(err, CoreError::ArtifactMissing(ref p) if *p == path),
            "expected ArtifactMissing({}), got {err:?}",
            path.display()
        );
    }

    #[test]
    fn load_non_notfound_io_error_is_not_reported_as_missing() {
        // Reading a directory as a file fails with a kind OTHER than NotFound, so
        // it must surface as Io — reporting ArtifactMissing here would misdiagnose.
        let dir = tempfile::tempdir().unwrap();
        let err = ContractInfo::load(dir.path()).unwrap_err();
        assert!(
            matches!(err, CoreError::Io(_)),
            "expected Io for a non-NotFound read failure, got {err:?}"
        );
    }

    #[test]
    fn load_rejects_an_oversized_artifact_before_reading_it() {
        // A `contract-info.json` whose reported size exceeds the cap is rejected
        // on the metadata check, before `read_to_string` can buffer it. The
        // sparse file (via `set_len`) supplies the large reported length without
        // allocating the bytes.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contract-info.json");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(super::super::MAX_ARTIFACT_BYTES + 1).unwrap();
        let err = ContractInfo::load(&path).unwrap_err();
        assert!(
            matches!(err, CoreError::MalformedArtifact { ref reason, .. } if reason.contains("maximum size")),
            "expected an oversize rejection carrying the limit, got {err:?}"
        );
    }

    #[test]
    fn load_malformed_json_is_malformed_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contract-info.json");
        std::fs::write(&path, "{ this is not valid json").unwrap();
        let err = ContractInfo::load(&path).unwrap_err();
        assert!(
            matches!(err, CoreError::MalformedArtifact { .. }),
            "expected MalformedArtifact, got {err:?}"
        );
    }

    #[test]
    fn missing_required_result_type_fails_loudly() {
        // A circuit with no result-type key (either spelling) must be a hard
        // deserialization error, never a silently-defaulted empty TypeRef.
        let broken = r#"{
          "compiler-version": "0.31.1",
          "language-version": "0.23.0",
          "runtime-version": "0.16.0",
          "circuits": [{ "name": "put", "pure": false, "proof": true, "arguments": [] }],
          "witnesses": [], "contracts": [], "ledger": []
        }"#;
        assert!(
            serde_json::from_str::<ContractInfo>(broken).is_err(),
            "a circuit missing its result-type must fail to deserialize"
        );
    }

    #[test]
    fn missing_required_version_fails_loudly() {
        // The version fields have no #[serde(default)]; an absent one is a hard error.
        let broken = r#"{
          "language-version": "0.23.0",
          "runtime-version": "0.16.0",
          "circuits": [], "witnesses": [], "contracts": [], "ledger": []
        }"#;
        assert!(
            serde_json::from_str::<ContractInfo>(broken).is_err(),
            "a missing compiler-version must fail to deserialize"
        );
    }

    #[test]
    fn circuit_result_type_accepts_the_space_spelling_too() {
        // Symmetry with Witness: a circuit spelled with a SPACE still deserializes
        // via the alias, rather than failing or silently defaulting.
        let spaced = r#"{
          "compiler-version": "0.31.1",
          "language-version": "0.23.0",
          "runtime-version": "0.16.0",
          "circuits": [{ "name": "put", "pure": true, "proof": false, "arguments": [],
            "result type": { "type-name": "Field" } }],
          "witnesses": [], "contracts": [], "ledger": []
        }"#;
        let ci: ContractInfo = serde_json::from_str(spaced).unwrap();
        assert_eq!(ci.circuits[0].result_type.type_name, "Field");
    }

    #[test]
    fn non_empty_tuple_result_maps_to_typescript_tuple() {
        // Live-verified shape: a non-empty tuple emits a PLURAL `types` array.
        let t = TypeRef {
            types: vec![TypeRef::named("Field"), TypeRef::named("Boolean")],
            ..TypeRef::named("Tuple")
        };
        assert_eq!(ts_type(&t), "[bigint, boolean]");
    }

    /// Verbatim `compact compile --skip-zk` output (compiler 0.31.1) for witnesses
    /// taking a `Vector<3, Field>`, a `struct Point { x: Field, y: Field }`, an
    /// `enum Colour { red, green, blue }`, and a `Vector<2, Point>`. The generated
    /// `index.d.ts` gives these witnesses, respectively, the TS parameter types
    /// `bigint[]`, `{ x: bigint, y: bigint }`, `number`, and
    /// `{ x: bigint, y: bigint }[]` — the ground truth the mappings below lock to.
    const REAL_COMPOSITE: &str = r#"{
      "compiler-version": "0.31.1",
      "language-version": "0.23.0",
      "runtime-version": "0.16.0",
      "circuits": [
        { "name": "run", "pure": false, "proof": true, "arguments": [],
          "result-type": { "type-name": "Tuple", "types": [] } }
      ],
      "witnesses": [
        { "name": "vec_w",
          "arguments": [{ "name": "v",
            "type": { "type-name": "Vector", "length": 3, "type": { "type-name": "Field" } } }],
          "result type": { "type-name": "Field" } },
        { "name": "struct_w",
          "arguments": [{ "name": "p",
            "type": { "type-name": "Struct", "name": "Point", "elements": [
              { "name": "x", "type": { "type-name": "Field" } },
              { "name": "y", "type": { "type-name": "Field" } }
            ] } }],
          "result type": { "type-name": "Field" } },
        { "name": "enum_w",
          "arguments": [{ "name": "c",
            "type": { "type-name": "Enum", "name": "Colour", "elements": ["red", "green", "blue"] } }],
          "result type": { "type-name": "Field" } },
        { "name": "vec_struct_w",
          "arguments": [{ "name": "vs",
            "type": { "type-name": "Vector", "length": 2, "type": {
              "type-name": "Struct", "name": "Point", "elements": [
                { "name": "x", "type": { "type-name": "Field" } },
                { "name": "y", "type": { "type-name": "Field" } }
              ] } } }],
          "result type": { "type-name": "Field" } }
      ],
      "contracts": [],
      "ledger": [{ "name": "n", "index": 0, "exported": true, "storage": "Counter" }]
    }"#;

    /// The composite JSON round-trips: the Vector's element type lands under the
    /// singular `type` key, the struct's fields under `elements` as `Field`
    /// members, and the enum's variants under `elements` as bare-string
    /// `Variant`s. This is what makes the precise TS mappings recoverable.
    #[test]
    fn composite_types_deserialize_from_real_compiler_output() {
        let ci: ContractInfo = serde_json::from_str(REAL_COMPOSITE).unwrap();

        let vec_ty = &ci.witnesses[0].arguments[0].ty;
        assert_eq!(vec_ty.type_name, "Vector");
        assert_eq!(vec_ty.length, Some(3));
        assert_eq!(vec_ty.element_type.as_ref().unwrap().type_name, "Field");

        let struct_ty = &ci.witnesses[1].arguments[0].ty;
        assert_eq!(struct_ty.type_name, "Struct");
        assert_eq!(struct_ty.name.as_deref(), Some("Point"));
        match &struct_ty.elements[0] {
            TypeElement::Field(f) => {
                assert_eq!(f.name, "x");
                assert_eq!(f.ty.type_name, "Field");
            }
            other => panic!("expected a struct Field, got {other:?}"),
        }

        let enum_ty = &ci.witnesses[2].arguments[0].ty;
        assert_eq!(enum_ty.type_name, "Enum");
        assert_eq!(enum_ty.name.as_deref(), Some("Colour"));
        match &enum_ty.elements[0] {
            TypeElement::Variant(v) => assert_eq!(v, "red"),
            other => panic!("expected an enum Variant, got {other:?}"),
        }
    }

    #[test]
    fn vector_param_maps_to_element_array() {
        // `Vector<3, Field>` -> `bigint[]`, exactly as the compiler's index.d.ts.
        let ci: ContractInfo = serde_json::from_str(REAL_COMPOSITE).unwrap();
        assert_eq!(ts_type(&ci.witnesses[0].arguments[0].ty), "bigint[]");
    }

    #[test]
    fn struct_param_maps_to_object_literal() {
        // `struct Point { x: Field, y: Field }` -> `{ x: bigint, y: bigint }`.
        let ci: ContractInfo = serde_json::from_str(REAL_COMPOSITE).unwrap();
        assert_eq!(
            ts_type(&ci.witnesses[1].arguments[0].ty),
            "{ x: bigint, y: bigint }"
        );
    }

    #[test]
    fn enum_param_maps_to_number() {
        // An `enum` witness parameter -> `number` (the compiler uses the ordinal).
        let ci: ContractInfo = serde_json::from_str(REAL_COMPOSITE).unwrap();
        assert_eq!(ts_type(&ci.witnesses[2].arguments[0].ty), "number");
    }

    #[test]
    fn nested_vector_of_struct_maps_precisely() {
        // `Vector<2, Point>` -> `{ x: bigint, y: bigint }[]` — recursion through
        // both the Vector element type and the nested struct fields.
        let ci: ContractInfo = serde_json::from_str(REAL_COMPOSITE).unwrap();
        assert_eq!(
            ts_type(&ci.witnesses[3].arguments[0].ty),
            "{ x: bigint, y: bigint }[]"
        );
    }

    #[test]
    fn empty_struct_maps_to_empty_object() {
        // A field-less struct has an empty `elements`; the compiler emits `{  }`
        // for it, and so must we (not `unknown` — an empty object is precise).
        let t = TypeRef {
            name: Some("Empty".to_string()),
            ..TypeRef::named("Struct")
        };
        assert_eq!(ts_type(&t), "{  }");
    }

    #[test]
    fn a_vector_missing_its_element_type_is_unknown_not_a_wrong_type() {
        // If the compiler ever stopped emitting the singular `type` key, the
        // element type is unrecoverable: emit the honest `unknown`, never a guess.
        let t = TypeRef {
            length: Some(3),
            ..TypeRef::named("Vector")
        };
        assert_eq!(ts_type(&t), "unknown");
    }

    #[test]
    fn missing_circuits_key_fails_loudly() {
        // Acceptance criterion: a contract-info.json missing `circuits` entirely
        // must be a hard error, NOT a silent deserialization to zero circuits.
        let drifted = r#"{
          "compiler-version": "0.31.1",
          "language-version": "0.23.0",
          "runtime-version": "0.16.0",
          "witnesses": [], "contracts": [], "ledger": []
        }"#;
        assert!(
            serde_json::from_str::<ContractInfo>(drifted).is_err(),
            "a missing `circuits` key must fail to deserialize, not read as circuit-less"
        );
    }

    #[test]
    fn missing_any_structural_key_fails_loudly() {
        // The same fail-loud stance covers witnesses/contracts/ledger too, so a
        // rename of any of the four structural keys surfaces instead of hiding.
        for absent in ["witnesses", "contracts", "ledger"] {
            let full = serde_json::json!({
                "compiler-version": "0.31.1",
                "language-version": "0.23.0",
                "runtime-version": "0.16.0",
                "circuits": [],
                "witnesses": [],
                "contracts": [],
                "ledger": []
            });
            let mut obj = full.as_object().unwrap().clone();
            obj.remove(absent);
            let drifted = serde_json::Value::Object(obj).to_string();
            assert!(
                serde_json::from_str::<ContractInfo>(&drifted).is_err(),
                "a missing `{absent}` key must fail to deserialize"
            );
        }
    }
}
