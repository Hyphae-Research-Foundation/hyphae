// SPDX-License-Identifier: Apache-2.0

//! Exhaustive injected-interruption recovery matrix for security mutations.

#[path = "../examples/security_crash_support.rs"]
mod security_crash_support;

use std::error::Error;

#[test]
fn every_public_security_mutation_recovers_at_every_real_commit_boundary()
-> Result<(), Box<dyn Error>> {
    security_crash_support::every_public_security_mutation_recovers_at_every_real_commit_boundary()
}

#[test]
fn every_offline_security_transition_recovers_at_every_real_commit_boundary()
-> Result<(), Box<dyn Error>> {
    security_crash_support::every_offline_security_transition_recovers_at_every_real_commit_boundary(
    )
}

#[test]
fn self_terminal_mutations_replay_after_the_actor_key_is_retired() -> Result<(), Box<dyn Error>> {
    security_crash_support::self_terminal_mutations_replay_after_the_actor_key_is_retired()
}
