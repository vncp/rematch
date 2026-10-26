//! Batch mode (SPEC "Batch mode", Build plan Phase 2): task definitions, isolated
//! rollouts, deterministic verifiers and the Harness² per-task worker.

pub mod rollout;
pub mod run;
pub mod substrate;
pub mod tasks;
pub mod verifier;
pub mod worker;

#[cfg(test)]
mod tests;
