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
use crate::workspace::is_single_normal_component;

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
    /// True when at least one circuit has a discovered prover key. A `--skip-zk`
    /// build emits none. (Tied to recognized key files, not merely a non-empty
    /// `keys/` dir, so a stray file is not a false positive.)
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

    let circuits: Vec<CircuitArtifact> = info
        .circuits
        .iter()
        .map(|c| {
            // A circuit name is formatted directly into artifact paths. A real
            // name is a plain identifier, but a crafted contract-info.json could
            // put `..`/separators here and escape `target_dir`. Only look up
            // artifacts when the name is a single normal path component — the
            // same guard the workspace uses for temp-file names.
            let safe = is_single_normal_component(&c.name);
            let art = |subdir: &str, ext: &str| -> Option<String> {
                if !safe {
                    return None;
                }
                rel(target_dir.join(format!("{subdir}/{}.{ext}", c.name)))
            };
            CircuitArtifact {
                name: c.name.clone(),
                proof: c.proof,
                zkir: art("zkir", "zkir"),
                bzkir: art("zkir", "bzkir"),
                prover_key: art("keys", "prover"),
                verifier_key: art("keys", "verifier"),
            }
        })
        .collect();

    let proving_keys = circuits.iter().any(|c| c.prover_key.is_some());

    Ok(Artifacts {
        contract_info: info,
        proving_keys,
        files,
        circuits,
    })
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), CoreError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        // Skip symlinks entirely. `Path::is_dir()` follows them, so a symlink to
        // an outside directory would leak external filenames into `files` (they
        // stay lexically prefixed by `root`), and a symlink cycle would recurse
        // unboundedly. `entry.file_type()` reports the link itself, not its
        // target. A real compiler output tree contains no symlinks.
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        let p = entry.path();
        if ft.is_dir() {
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

    #[test]
    fn a_crafted_circuit_name_cannot_escape_the_target_dir() {
        // A malicious contract-info.json names a circuit `../../secret`. Even
        // with a matching file planted OUTSIDE target_dir (exactly where the
        // `..` would resolve), scan must not surface a path to it: an unsafe
        // name yields None for every artifact field.
        let d = tempfile::tempdir().unwrap();
        let t = d.path().join("target");
        std::fs::create_dir_all(t.join("compiler")).unwrap();
        let ci = r#"{
          "compiler-version":"0.31.1","language-version":"0.23.0","runtime-version":"0.16.0",
          "circuits":[{"name":"../../secret","pure":false,"proof":true,"arguments":[],"result-type":{"type-name":"Field"}}],
          "witnesses":[],"contracts":[],"ledger":[]
        }"#;
        std::fs::write(t.join("compiler/contract-info.json"), ci).unwrap();
        // `target/zkir/../../secret.zkir` resolves to `<tempdir>/secret.zkir`.
        std::fs::write(d.path().join("secret.zkir"), "{}").unwrap();

        let a = scan(&t).unwrap();
        let c = &a.circuits[0];
        assert_eq!(c.name, "../../secret");
        assert!(
            c.zkir.is_none(),
            "an unsafe circuit name must not resolve to an out-of-tree file"
        );
        assert!(c.bzkir.is_none() && c.prover_key.is_none() && c.verifier_key.is_none());
        assert!(!a.proving_keys);
    }

    #[cfg(unix)]
    #[test]
    fn collect_does_not_follow_symlinks_out_of_the_tree() {
        // A symlink inside the target pointing at an outside dir must not leak
        // that dir's filenames into `files`, nor be recursed into.
        let d = tempfile::tempdir().unwrap();
        let t = d.path().join("target");
        std::fs::create_dir_all(t.join("compiler")).unwrap();
        std::fs::write(t.join("compiler/contract-info.json"), CI).unwrap();
        let outside = d.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("leak.txt"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, t.join("link")).unwrap();

        let a = scan(&t).unwrap();
        assert!(
            !a.files.iter().any(|f| f.contains("leak.txt")),
            "a symlinked-out dir must not leak into files: {:?}",
            a.files
        );
    }
}
