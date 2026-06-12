//! Compile-time UI tests for the `#[causal::event]` macro.
//!
//! The load-bearing cases are the `compile_fail` ones: omitting
//! `subject_id` used to silently default every variant to the nil
//! stream — the trap that mass-produces fan-in aggregates no
//! per-stream read can serve (a fact's stream identity is the id its
//! aggregates fold by). The macro must reject the omission with an
//! error that teaches the fix, and require an explicit `no_subject`
//! opt-in for genuinely streamless facts.

#[test]
fn event_macro_ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/event_with_subject_id.rs");
    t.pass("tests/ui/event_no_subject_opt_in.rs");
    t.pass("tests/ui/event_timeless_facts_compile.rs");
    t.pass("tests/ui/event_subjectless_omission_legal.rs");
    t.pass("tests/ui/event_no_subject_keeps_timestamps.rs");
    t.pass("tests/ui/event_workflow_root.rs");
    t.compile_fail("tests/ui/event_struct_missing_subject_id.rs");
    t.compile_fail("tests/ui/event_enum_retracted.rs");
    t.compile_fail("tests/ui/event_subject_id_no_subject_conflict.rs");
    t.compile_fail("tests/ui/event_struct_subject_id_typo.rs");
    t.compile_fail("tests/ui/event_explicit_occurred_at_field_missing.rs");
    t.compile_fail("tests/ui/event_workflow_root_missing_field.rs");
}
