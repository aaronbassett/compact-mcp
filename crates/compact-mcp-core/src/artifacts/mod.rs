pub mod contract_info;
pub mod zkir;

pub use contract_info::{Argument, Circuit, ContractInfo, LedgerField, TypeRef, Witness, ts_type};
// Re-export the zkir TYPES for symmetry with contract_info. The `stats` free
// function stays module-qualified (`zkir::stats`) so a future artifact module
// with its own `stats` cannot collide at this level.
pub use zkir::{Instruction, Zkir, ZkirStats, ZkirVersion};

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::CoreError;

#[derive(Debug, Clone, Serialize)]
pub struct CircuitArtifact {
    pub name: String,
    pub proof: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zkir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bzkir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prover_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verifier_key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Artifacts {
    pub contract_info: ContractInfo,
    /// True when `keys/` contains anything. A `--skip-zk` build never creates it.
    pub proving_keys: bool,
    /// Every file under the target dir, relative. We DISCOVER rather than assume.
    pub files: Vec<String>,
    pub circuits: Vec<CircuitArtifact>,
}

pub fn scan(target_dir: &Path) -> Result<Artifacts, CoreError> {
    if !target_dir.is_dir() {
        return Err(CoreError::ArtifactMissing(target_dir.to_path_buf()));
    }
    let info = ContractInfo::load(&target_dir.join("compiler/contract-info.json"))?;

    let mut files = Vec::new();
    collect(target_dir, target_dir, &mut files)?;
    files.sort();

    let rel = |p: PathBuf| -> Option<String> {
        p.exists()
            .then(|| {
                p.strip_prefix(target_dir)
                    .ok()
                    .map(|r| r.to_string_lossy().into_owned())
            })
            .flatten()
    };

    let circuits = info
        .circuits
        .iter()
        .map(|c| CircuitArtifact {
            name: c.name.clone(),
            proof: c.proof,
            zkir: rel(target_dir.join(format!("zkir/{}.zkir", c.name))),
            bzkir: rel(target_dir.join(format!("zkir/{}.bzkir", c.name))),
            prover_key: rel(target_dir.join(format!("keys/{}.prover", c.name))),
            verifier_key: rel(target_dir.join(format!("keys/{}.verifier", c.name))),
        })
        .collect();

    let proving_keys = std::fs::read_dir(target_dir.join("keys"))
        .map(|mut d| d.next().is_some())
        .unwrap_or(false);

    Ok(Artifacts {
        contract_info: info,
        proving_keys,
        files,
        circuits,
    })
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), CoreError> {
    for entry in std::fs::read_dir(dir)? {
        let p = entry?.path();
        if p.is_dir() {
            collect(root, &p, out)?;
        } else if let Ok(r) = p.strip_prefix(root) {
            out.push(r.to_string_lossy().into_owned());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(p: &std::path::Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "{}").unwrap();
    }

    const CI: &str = r#"{
      "compiler-version":"0.31.1","language-version":"0.23.0","runtime-version":"0.16.0",
      "circuits":[
        {"name":"increment","pure":false,"proof":true,"arguments":[],"result-type":{"type-name":"Tuple","types":[]}},
        {"name":"reveal","pure":false,"proof":false,"arguments":[],"result-type":{"type-name":"Field"}}
      ],
      "witnesses":[],"contracts":[],"ledger":[]
    }"#;

    #[test]
    fn a_skip_zk_build_has_zkir_only_for_proof_circuits_and_no_keys() {
        let d = tempfile::tempdir().unwrap();
        let t = d.path();
        std::fs::create_dir_all(t.join("compiler")).unwrap();
        std::fs::write(t.join("compiler/contract-info.json"), CI).unwrap();
        touch(&t.join("contract/index.d.ts"));
        touch(&t.join("zkir/increment.zkir")); // `reveal` has proof:false => no zkir

        let a = scan(t).unwrap();
        assert!(!a.proving_keys);
        let inc = a.circuits.iter().find(|c| c.name == "increment").unwrap();
        let rev = a.circuits.iter().find(|c| c.name == "reveal").unwrap();
        assert!(inc.zkir.is_some());
        assert!(inc.prover_key.is_none());
        assert!(rev.zkir.is_none(), "proof:false circuits emit no zkir");
        assert!(a.files.iter().any(|f| f.ends_with("index.d.ts")));
    }

    #[test]
    fn a_full_build_reports_keys_and_bzkir() {
        let d = tempfile::tempdir().unwrap();
        let t = d.path();
        std::fs::create_dir_all(t.join("compiler")).unwrap();
        std::fs::write(t.join("compiler/contract-info.json"), CI).unwrap();
        touch(&t.join("zkir/increment.zkir"));
        touch(&t.join("zkir/increment.bzkir"));
        touch(&t.join("keys/increment.prover"));
        touch(&t.join("keys/increment.verifier"));

        let a = scan(t).unwrap();
        assert!(a.proving_keys);
        let inc = &a.circuits[0];
        assert!(inc.bzkir.is_some() && inc.prover_key.is_some() && inc.verifier_key.is_some());
    }

    #[test]
    fn a_missing_target_dir_is_a_clear_error() {
        let d = tempfile::tempdir().unwrap();
        assert!(matches!(
            scan(&d.path().join("nope")),
            Err(crate::CoreError::ArtifactMissing(_))
        ));
    }
}
