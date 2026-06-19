//! The shared benchmark/probe task set: narrow, repo-specific questions whose answers live in one
//! or two source files — exactly the case where injecting the relevant symbol should save the model
//! a file read. Used by both `bench-lattice` (live token measurement) and `probe-retrieve`
//! (offline injection inspection) so the two never drift.

pub struct BenchTask {
    pub id: &'static str,
    pub prompt: &'static str,
}

pub const TASKS: &[BenchTask] = &[
    BenchTask {
        id: "T1-usage-fields",
        prompt: "List every field of the `Usage` struct in the forge-types crate. Answer concisely, then stop.",
    },
    BenchTask {
        id: "T2-inject-budget",
        prompt: "In forge-core, what value does the `inject_budget` function return when the BudgetStatus is the most constrained variant, given a base of 1500? Answer with the number and stop.",
    },
    BenchTask {
        id: "T3-record-usage",
        prompt: "Which method on the Store type in forge-store records per-message token usage, and which SQL table does it INSERT into? Answer in one line and stop.",
    },
    // Deliberately names no symbol (tests the prose-fallback retrieval path). The answer lives in
    // forge-index `retrieve.rs`; the prompt asks about behaviour, not a function name.
    BenchTask {
        id: "T4-prompt-tokens",
        prompt: "In forge-index retrieval (the retrieve.rs source), what is the minimum character length a prompt token must have to be used as a query term, and name one stopword that is dropped? Answer in one line and stop.",
    },
    BenchTask {
        id: "T5-permission-modes",
        prompt: "Name the variants of the PermissionMode enum in forge-types. Answer with just the variant names and stop.",
    },
];
