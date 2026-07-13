pub mod contract_info;
pub mod scaffold;
pub mod zkir;

pub use contract_info::{Argument, Circuit, ContractInfo, LedgerField, TypeRef, Witness, ts_type};
// Re-export the zkir TYPES for symmetry with contract_info. The `stats` free
// function stays module-qualified (`zkir::stats`) so a future artifact module
// with its own `stats` cannot collide at this level.
pub use zkir::{Instruction, Zkir, ZkirStats, ZkirVersion};

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::CoreError;
use crate::workspace::is_single_normal_component;

/// Largest compiler-produced artifact (`.zkir`, `contract-info.json`) we will
/// read into memory, in bytes (64 MiB). Deliberately far above `MAX_SOURCE_BYTES`
/// — a large circuit's `.zkir` dwarfs its hand-written source — while still
/// bounding the peak memory a single artifact read (plus its `from_utf8` + JSON
/// parse) can cost. This is a safety ceiling on trusted, in-workspace files, not
/// an operational tuning knob, so it is a constant rather than a CLI flag,
/// mirroring `MAX_SOURCE_BYTES`.
pub const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;

/// Read a compiler artifact to a string, bounding the read at
/// [`MAX_ARTIFACT_BYTES`] so an oversized artifact is never buffered whole just
/// to be rejected — mirroring the source-path pre-check in the server's
/// `read_input`.
///
/// The bound is enforced on the ACTUAL bytes read (`Read::take`), not merely on
/// `metadata().len()`: a file that grows after the stat, or a special file whose
/// `len()` under-reports, is still capped. The `metadata().len()` check is kept
/// only as a cheap early reject that avoids opening an obviously huge file.
///
/// A genuine "not found" surfaces as [`CoreError::ArtifactMissing`]; every other
/// read failure (permission, is-a-directory, non-UTF-8) surfaces as
/// [`CoreError::Io`], so a present-but-unreadable file is never misreported as
/// missing. An over-limit file surfaces as [`CoreError::MalformedArtifact`] (no
/// dedicated variant: from the caller's view an unreadably huge artifact is as
/// unusable as a malformed one, and the reason string says which).
pub(crate) fn read_to_string_capped(path: &Path) -> Result<String, CoreError> {
    let map_io = |e: std::io::Error| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CoreError::ArtifactMissing(path.to_path_buf())
        } else {
            CoreError::Io(e)
        }
    };
    let over_limit = || CoreError::MalformedArtifact {
        path: path.to_path_buf(),
        reason: format!("artifact exceeds maximum size (limit {MAX_ARTIFACT_BYTES} bytes)"),
    };

    // Cheap early reject on the stat'd size. A file that lies here (or grows after
    // this stat) is still caught by the `take` bound below — this only avoids
    // opening an obviously-huge file.
    if std::fs::metadata(path).map_err(map_io)?.len() > MAX_ARTIFACT_BYTES {
        return Err(over_limit());
    }

    // The REAL bound: read at most `MAX_ARTIFACT_BYTES + 1` bytes. Reading one
    // byte past the cap lets us tell "exactly at the cap" from "over" without ever
    // pulling an unbounded amount into memory (closing the stat-then-read TOCTOU).
    let file = std::fs::File::open(path).map_err(map_io)?;
    let mut buf = Vec::new();
    file.take(MAX_ARTIFACT_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(map_io)?;
    if buf.len() as u64 > MAX_ARTIFACT_BYTES {
        return Err(over_limit());
    }
    // Preserve `read_to_string`'s contract: non-UTF-8 content is an
    // `Io(InvalidData)` error, NOT a missing or malformed artifact.
    String::from_utf8(buf).map_err(|e| {
        CoreError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            e.utf8_error(),
        ))
    })
}

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

    // Resolve each candidate artifact and confirm the REAL path stays inside
    // target_dir, mirroring `Workspace::resolve`'s canonicalize-and-contain
    // invariant. `canonicalize` follows symlinks and fails on a non-existent
    // path, so it doubles as the existence check AND rejects a symlink planted
    // at (or above) the expected artifact path that points out of the workspace
    // — a boundary `p.exists()` alone would follow straight through.
    let canonical_root = target_dir.canonicalize().ok();
    let rel = |p: PathBuf| -> Option<String> {
        let root = canonical_root.as_ref()?;
        if !p.canonicalize().ok()?.starts_with(root) {
            return None;
        }
        p.strip_prefix(target_dir)
            .ok()
            .map(|r| r.to_string_lossy().into_owned())
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
    fn a_symlinked_artifact_pointing_outside_is_not_surfaced() {
        // A safe circuit name, but its expected `.zkir` is a SYMLINK pointing at
        // a file outside the workspace. `exists()` would follow it; scan must not
        // surface (and zkir_stats must not read) an out-of-tree target.
        let d = tempfile::tempdir().unwrap();
        let t = d.path().join("target");
        std::fs::create_dir_all(t.join("compiler")).unwrap();
        std::fs::create_dir_all(t.join("zkir")).unwrap();
        std::fs::write(t.join("compiler/contract-info.json"), CI).unwrap();
        let outside = d.path().join("secret.zkir");
        std::fs::write(&outside, "{}").unwrap();
        std::os::unix::fs::symlink(&outside, t.join("zkir/increment.zkir")).unwrap();

        let a = scan(&t).unwrap();
        let inc = a.circuits.iter().find(|c| c.name == "increment").unwrap();
        assert!(
            inc.zkir.is_none(),
            "a symlinked artifact pointing outside the workspace must not be surfaced"
        );
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
