//! Compile-time guarantees. `TRYBUILD=overwrite cargo test --test ui` refreshes the .stderr snapshots.
#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/pass/*.rs");
    t.compile_fail("tests/ui/fail/*.rs");
}
