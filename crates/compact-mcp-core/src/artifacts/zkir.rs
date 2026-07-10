use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::CoreError;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ZkirVersion {
    pub major: u32,
    pub minor: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Instruction {
    pub op: String,
    /// Operand fields vary per opcode; keep them rather than discard them.
    #[serde(flatten)]
    pub rest: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Zkir {
    pub version: ZkirVersion,
    pub do_communications_commitment: bool,
    pub num_inputs: u64,
    /// Intentionally NOT `#[serde(default)]`: a real `.zkir` always emits this
    /// key (and the file only exists for a `proof: true` circuit), so an absent
    /// `instructions` means a corrupt/wrong file and should fail loudly.
    pub instructions: Vec<Instruction>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ZkirStats {
    pub circuit: String,
    pub instruction_count: usize,
    pub num_inputs: u64,
    pub version: String,
    pub opcodes: BTreeMap<String, usize>,
}

impl ZkirStats {
    pub fn from_zkir(circuit: &str, z: &Zkir) -> Self {
        let mut opcodes = BTreeMap::new();
        for i in &z.instructions {
            *opcodes.entry(i.op.clone()).or_insert(0) += 1;
        }
        Self {
            circuit: circuit.to_string(),
            instruction_count: z.instructions.len(),
            num_inputs: z.num_inputs,
            version: format!("{}.{}", z.version.major, z.version.minor),
            opcodes,
        }
    }
}

pub fn stats(circuit: &str, path: &Path) -> Result<ZkirStats, CoreError> {
    // Only a genuine "not found" is ArtifactMissing; other io failures surface as Io.
    let text = std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CoreError::ArtifactMissing(path.to_path_buf())
        } else {
            CoreError::Io(e)
        }
    })?;
    let z: Zkir = serde_json::from_str(&text).map_err(|e| CoreError::MalformedArtifact {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    Ok(ZkirStats::from_zkir(circuit, &z))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a real `zkir/increment.zkir`.
    const REAL: &str = r#"{
      "version": { "major": 2, "minor": 0 },
      "do_communications_commitment": true,
      "num_inputs": 0,
      "instructions": [
        { "op": "load_imm", "imm": "01" },
        { "op": "load_imm", "imm": "70" },
        { "op": "add" },
        { "op": "add" }
      ]
    }"#;

    #[test]
    fn counts_instructions_and_histograms_opcodes() {
        let z: Zkir = serde_json::from_str(REAL).unwrap();
        let s = ZkirStats::from_zkir("increment", &z);
        assert_eq!(s.instruction_count, 4);
        assert_eq!(s.num_inputs, 0);
        assert_eq!(s.version, "2.0");
        assert_eq!(s.opcodes["load_imm"], 2);
        assert_eq!(s.opcodes["add"], 2);
    }

    #[test]
    fn unknown_instruction_fields_do_not_break_parsing() {
        let z: Zkir = serde_json::from_str(REAL).unwrap();
        assert_eq!(z.instructions[0].op, "load_imm");
        assert_eq!(z.instructions[0].rest["imm"], "01");
        assert!(z.do_communications_commitment);
    }

    #[test]
    fn no_operand_op_has_empty_rest_and_null_operand_is_preserved() {
        // Live-observed shapes: `{"op":"add"}` (no operands) and
        // `{"op":"private_input","guard":null}` (null-valued operand). Flatten
        // must capture both without leaking `op` into `rest`.
        let probe = r#"{"version":{"major":2,"minor":0},"do_communications_commitment":true,
          "num_inputs":0,"instructions":[{"op":"add"},{"op":"private_input","guard":null}]}"#;
        let z: Zkir = serde_json::from_str(probe).unwrap();
        assert_eq!(z.instructions[0].op, "add");
        assert!(z.instructions[0].rest.is_empty());
        assert_eq!(
            z.instructions[1].rest.get("guard"),
            Some(&serde_json::Value::Null)
        );
        assert!(!z.instructions[1].rest.contains_key("op"));
    }

    #[test]
    fn stats_reads_a_valid_file_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("increment.zkir");
        std::fs::write(&path, REAL).unwrap();
        let s = stats("increment", &path).unwrap();
        assert_eq!(s.instruction_count, 4);
        assert_eq!(s.opcodes["load_imm"], 2);
        assert_eq!(s.version, "2.0");
    }

    #[test]
    fn stats_missing_file_is_artifact_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.zkir");
        let err = stats("x", &path).unwrap_err();
        assert!(
            matches!(err, CoreError::ArtifactMissing(ref p) if *p == path),
            "expected ArtifactMissing({}), got {err:?}",
            path.display()
        );
    }

    #[test]
    fn stats_non_notfound_io_error_is_not_reported_as_missing() {
        // Reading a directory as a file fails with a kind OTHER than NotFound, so
        // it must surface as Io — reporting ArtifactMissing here would misdiagnose.
        let dir = tempfile::tempdir().unwrap();
        let err = stats("x", dir.path()).unwrap_err();
        assert!(
            matches!(err, CoreError::Io(_)),
            "expected Io for a non-NotFound read failure, got {err:?}"
        );
    }

    #[test]
    fn stats_malformed_zkir_is_malformed_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.zkir");
        std::fs::write(&path, "{ not json").unwrap();
        let err = stats("x", &path).unwrap_err();
        assert!(
            matches!(err, CoreError::MalformedArtifact { .. }),
            "expected MalformedArtifact, got {err:?}"
        );
    }
}
