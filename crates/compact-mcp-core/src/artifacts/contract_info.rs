use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::CoreError;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TypeRef {
    #[serde(rename = "type-name")]
    pub type_name: String,
    /// `Bytes<N>` carries `length` (byte width). `Vector<N, T>` ALSO emits a
    /// `length` key here (the element count — a different meaning); the Vector's
    /// element type is a singular `type` key the compiler emits that this struct
    /// does not model, so a Vector round-trips with only its length preserved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,
    /// `Uint<N>` carries `maxval` — the maximum value, NOT the bit width.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maxval: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<TypeRef>,
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
    #[serde(default)]
    pub circuits: Vec<Circuit>,
    /// Only witnesses that some circuit actually CALLS appear here. A declared
    /// but unreferenced witness is absent — compare with the `symbols` tool.
    #[serde(default)]
    pub witnesses: Vec<Witness>,
    #[serde(default)]
    pub contracts: Vec<serde_json::Value>,
    #[serde(default)]
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

/// Compact type -> TypeScript type, as the compiler itself emits in `index.d.ts`.
/// Verified: `Field`/`Uint<N>` -> `bigint`, `Bytes<N>` -> `Uint8Array`,
/// `Boolean` -> `boolean`, `[]` -> `[]`.
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
        _ => "unknown".to_string(),
    }
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
}
