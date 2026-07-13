use compact_mcp_core::{analyze, artifacts};

const SRC: &str = "pragma language_version >= 0.23;\n\
                   import CompactStandardLibrary;\n\
                   export ledger round: Counter;\n\
                   sealed ledger owner: Bytes<32>;\n\
                   witness secret_value(): Field;\n\
                   export pure circuit double(x: Field): Field { return x + x; }\n\
                   export circuit increment(): [] { round.increment(1); }\n\
                   struct Point { x: Field, y: Field }\n\
                   enum Colour { Red, Green }\n";

#[test]
fn diagnostics_shape_is_stable() {
    insta::assert_json_snapshot!(analyze::diagnostics(
        "ledger count Field;",
        "a.compact",
        None
    ));
}

#[test]
fn symbols_shape_is_stable() {
    insta::assert_json_snapshot!(analyze::symbols(SRC));
}

#[test]
fn ast_shape_is_stable() {
    insta::assert_json_snapshot!(analyze::ast_json(SRC));
}

// Exercises the two omission fixes (issue #5): declarations nested in a `module { … }`
// block, and the `export { … }` list form (both an inline top-level list and a
// module-scoped one). Locks that nested members are listed with a `module` path and
// that list-exported declarations report `exported: true`.
const NESTED_SRC: &str = "pragma language_version >= 0.23;\n\
                          witness top_secret(): Field;\n\
                          circuit top_run(): [] {}\n\
                          export { top_run };\n\
                          module Vault {\n\
                            export ledger balance: Counter;\n\
                            witness key(): Field;\n\
                            circuit spend(): [] {}\n\
                            export { key };\n\
                          }\n";

#[test]
fn symbols_module_and_export_list_shape_is_stable() {
    insta::assert_json_snapshot!(analyze::symbols(NESTED_SRC));
}

#[test]
fn ast_module_and_export_list_shape_is_stable() {
    insta::assert_json_snapshot!(analyze::ast_json(NESTED_SRC));
}

#[test]
fn stats_shape_is_stable() {
    insta::assert_json_snapshot!(analyze::stats(SRC), { ".parse_time_ms" => "[elapsed]" });
}

#[test]
fn zkir_stats_shape_is_stable() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("increment.zkir");
    std::fs::write(
        &p,
        r#"{"version":{"major":2,"minor":0},"do_communications_commitment":true,
            "num_inputs":0,"instructions":[{"op":"load_imm","imm":"01"},{"op":"add"}]}"#,
    )
    .unwrap();
    insta::assert_json_snapshot!(artifacts::zkir::stats("increment", &p).unwrap());
}

#[test]
fn witness_scaffold_output_is_stable() {
    let info: artifacts::ContractInfo = serde_json::from_str(
        r#"{"compiler-version":"0.31.1","language-version":"0.23.0","runtime-version":"0.16.0",
            "circuits":[],"contracts":[],"ledger":[],
            "witnesses":[
              {"name":"lookup","arguments":[
                 {"name":"key","type":{"type-name":"Bytes","length":32}},
                 {"name":"idx","type":{"type-name":"Uint","maxval":65535}}],
               "result type":{"type-name":"Field"}}]}"#,
    )
    .unwrap();
    // Plain text snapshot — this one is read by humans, not parsed.
    insta::assert_snapshot!(artifacts::scaffold::witnesses_ts(&info));
}

// Locks the composite-type scaffold (issue #11) against real `compact compile
// --skip-zk` output: `Vector<3, Field>` -> `bigint[]`, `struct Point` ->
// `{ x: bigint, y: bigint }`, `enum Colour` -> `number`, and `Vector<2, Point>`
// -> `{ x: bigint, y: bigint }[]`. The witness-parameter JSON below is verbatim
// compiler 0.31.1 output, and each generated TS parameter type matches the
// witness signatures the same build wrote to `index.d.ts`.
#[test]
fn composite_witness_scaffold_output_is_stable() {
    let info: artifacts::ContractInfo = serde_json::from_str(
        r#"{"compiler-version":"0.31.1","language-version":"0.23.0","runtime-version":"0.16.0",
            "circuits":[],"contracts":[],"ledger":[],
            "witnesses":[
              {"name":"vec_w","arguments":[
                 {"name":"v","type":{"type-name":"Vector","length":3,"type":{"type-name":"Field"}}}],
               "result type":{"type-name":"Field"}},
              {"name":"struct_w","arguments":[
                 {"name":"p","type":{"type-name":"Struct","name":"Point","elements":[
                    {"name":"x","type":{"type-name":"Field"}},
                    {"name":"y","type":{"type-name":"Field"}}]}}],
               "result type":{"type-name":"Field"}},
              {"name":"enum_w","arguments":[
                 {"name":"c","type":{"type-name":"Enum","name":"Colour","elements":["red","green","blue"]}}],
               "result type":{"type-name":"Field"}},
              {"name":"vec_struct_w","arguments":[
                 {"name":"vs","type":{"type-name":"Vector","length":2,"type":{
                    "type-name":"Struct","name":"Point","elements":[
                      {"name":"x","type":{"type-name":"Field"}},
                      {"name":"y","type":{"type-name":"Field"}}]}}}],
               "result type":{"type-name":"Field"}}]}"#,
    )
    .unwrap();
    insta::assert_snapshot!(artifacts::scaffold::witnesses_ts(&info));
}
