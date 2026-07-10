use super::{ContractInfo, ts_type};

/// Emit a TypeScript witness implementation stub. Types are taken from
/// `contract-info.json`, which is the compiler's own model, so the stub matches
/// the generated `Witnesses<PS>` type by construction.
pub fn witnesses_ts(info: &ContractInfo) -> String {
    let mut s = String::new();
    s.push_str("import type { Witnesses } from './contract/index.js';\n\n");
    s.push_str("export type PrivateState = Record<string, never>;\n\n");

    if info.witnesses.is_empty() {
        s.push_str(
            "// No witnesses are referenced by any circuit in this contract.\n\
             // Note: contract-info.json omits declared-but-unused witnesses;\n\
             // run the `symbols` tool to see every declaration in the source.\n\n",
        );
    }

    s.push_str("export const witnesses: Witnesses<PrivateState> = {\n");
    for w in &info.witnesses {
        let params: Vec<String> = w
            .arguments
            .iter()
            // The compiler suffixes every parameter with `_0`.
            .map(|a| format!("{}_0", a.name))
            .collect();

        let typed: Vec<String> = w
            .arguments
            .iter()
            .map(|a| format!("{}_0: {}", a.name, ts_type(&a.ty)))
            .collect();

        let ret = ts_type(&w.result_type);
        let sig_args = if typed.is_empty() {
            String::new()
        } else {
            format!(", {}", typed.join(", "))
        };

        s.push_str(&format!(
            "  // {}(context: WitnessContext<Ledger, PrivateState>{}): [PrivateState, {}]\n",
            w.name, sig_args, ret
        ));

        let call_args = if params.is_empty() {
            String::new()
        } else {
            format!(", {}", params.join(", "))
        };
        s.push_str(&format!("  {}(context{}) {{\n", w.name, call_args));
        s.push_str(&format!(
            "    throw new Error('witness `{}` is not implemented');\n  }},\n",
            w.name
        ));
    }
    s.push_str("};\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const CI: &str = r#"{
      "compiler-version":"0.31.1","language-version":"0.23.0","runtime-version":"0.16.0",
      "circuits":[],"contracts":[],"ledger":[],
      "witnesses":[
        {"name":"lookup","arguments":[
          {"name":"key","type":{"type-name":"Bytes","length":32}},
          {"name":"idx","type":{"type-name":"Uint","maxval":65535}}],
         "result type":{"type-name":"Field"}},
        {"name":"flag","arguments":[],"result type":{"type-name":"Boolean"}}
      ]}"#;

    #[test]
    fn generates_a_stub_matching_the_generated_witnesses_type() {
        let info: ContractInfo = serde_json::from_str(CI).unwrap();
        let ts = witnesses_ts(&info);

        // Signature comments mirror `index.d.ts`.
        assert!(ts.contains("key_0: Uint8Array"), "{ts}");
        assert!(ts.contains("idx_0: bigint"), "{ts}");
        assert!(ts.contains("[PrivateState, bigint]"), "{ts}");
        assert!(ts.contains("[PrivateState, boolean]"), "{ts}");

        // Implementation bodies.
        assert!(ts.contains("lookup(context, key_0, idx_0) {"), "{ts}");
        assert!(ts.contains("flag(context) {"), "{ts}");
        assert!(
            ts.contains("export const witnesses: Witnesses<PrivateState>"),
            "{ts}"
        );
        assert!(ts.contains("not implemented"), "{ts}");
    }

    #[test]
    fn a_contract_with_no_referenced_witnesses_says_so() {
        let info: ContractInfo = serde_json::from_str(
            r#"{"compiler-version":"0.31.1","language-version":"0.23.0","runtime-version":"0.16.0",
                "circuits":[],"witnesses":[],"contracts":[],"ledger":[]}"#,
        )
        .unwrap();
        let ts = witnesses_ts(&info);
        assert!(ts.contains("No witnesses are referenced"), "{ts}");
    }

    /// Guards the `_0` parameter-suffix assumption against a compiler change.
    #[tokio::test]
    #[cfg_attr(not(feature = "toolchain-tests"), ignore)]
    async fn generated_param_names_match_the_real_index_d_ts() {
        let src = "pragma language_version >= 0.23;\nimport CompactStandardLibrary;\n\
                   export ledger n: Counter;\n\
                   witness lookup(key: Bytes<32>, idx: Uint<16>): Field;\n\
                   export circuit put(): [] { const f = lookup(default<Bytes<32>>, 7); n.increment(1); }\n";
        let d = tempfile::tempdir().unwrap();
        let w = crate::Workspace::new(d.path()).unwrap();
        std::fs::write(d.path().join("c.compact"), src).unwrap();

        let tc = crate::Toolchain::new("compact", None);
        let req = crate::toolchain::compile::CompileRequest {
            source: w.resolve("c.compact").unwrap(),
            target_dir: w.resolve("out").unwrap(),
            skip_zk: true,
            no_communications_commitment: false,
            source_root: None,
        };
        let out = tc
            .compile(
                &req,
                tokio_util::sync::CancellationToken::new(),
                std::time::Duration::from_secs(300),
            )
            .await
            .unwrap();
        assert!(out.ok, "{:?}", out.diagnostics);

        let dts = std::fs::read_to_string(d.path().join("out/contract/index.d.ts")).unwrap();
        let info = ContractInfo::load(&d.path().join("out/compiler/contract-info.json")).unwrap();

        for w in &info.witnesses {
            for a in &w.arguments {
                let expected = format!("{}_0", a.name);
                assert!(
                    dts.contains(&expected),
                    "compiler no longer suffixes params with `_0`: expected {expected} in\n{dts}"
                );
            }
        }
    }
}
