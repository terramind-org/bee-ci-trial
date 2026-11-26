use bee_proto::v1::{AgentRunBudget, SwarmAttemptOutcome, SwarmState, SwarmTask, SwarmTaskState};
use prost::Message;

use crate::error::{ForgeError, Result};

/// Private join authorization data alongside the public record.
#[derive(Clone, PartialEq, Message)]
pub(super) struct Record {
    #[prost(message, required, tag = "1")]
    pub swarm: bee_proto::v1::Swarm,
    #[prost(message, repeated, tag = "2")]
    pub applicants: Vec<Applicant>,
    #[prost(string, tag = "3")]
    pub lead_token_id: String,
}

#[derive(Clone, PartialEq, Message)]
pub(super) struct Applicant {
    #[prost(string, tag = "1")]
    pub join_id: String,
    #[prost(string, repeated, tag = "2")]
    pub chain: Vec<String>,
}

pub(super) fn invalid_state(message: &str) -> ForgeError {
    ForgeError::invalid_state(message).with_subcode("swarm_state")
}

pub(super) fn transition(from: SwarmState, to: SwarmState) -> Result<()> {
    use SwarmState::*;
    if matches!(
        (from, to),
        (Planning | Paused, Working)
            | (Working, Paused)
            | (Planning | Working | Paused, Stopped | Finished)
    ) {
        Ok(())
    } else {
        Err(invalid_state("Swarm cannot move to that state"))
    }
}

pub(super) fn end_attempt(task: &mut SwarmTask, outcome: SwarmAttemptOutcome) {
    task.state = if outcome == SwarmAttemptOutcome::Finished {
        SwarmTaskState::Finished
    } else {
        SwarmTaskState::Empty
    } as i32;
    task.run_id.clear();
}

/// A zero in a run limit means unlimited, so any unlimited member makes the sum unlimited.
pub(super) fn add_budget(a: AgentRunBudget, b: AgentRunBudget) -> AgentRunBudget {
    fn sum(a: u64, b: u64) -> u64 {
        if a == 0 || b == 0 {
            0
        } else {
            a.saturating_add(b)
        }
    }
    AgentRunBudget {
        max_input_tokens: sum(a.max_input_tokens, b.max_input_tokens),
        max_output_tokens: sum(a.max_output_tokens, b.max_output_tokens),
        max_usd_micros: sum(a.max_usd_micros, b.max_usd_micros),
        max_duration_seconds: sum(a.max_duration_seconds, b.max_duration_seconds),
        max_changesets: sum(a.max_changesets, b.max_changesets),
        max_landed: sum(a.max_landed, b.max_landed),
    }
}

pub(super) fn line(field: &str, text: &str, max: usize) -> Result<()> {
    if text.trim().is_empty() {
        return Err(ForgeError::missing(field));
    }
    if text.chars().count() > max || text.chars().any(char::is_control) {
        return Err(ForgeError::invalid(
            "swarm_text",
            format!("Invalid {field}"),
        ));
    }
    Ok(())
}

pub(super) fn paths(paths: &mut Vec<Vec<u8>>) -> Result<()> {
    if paths.len() > 128
        || paths.iter().any(|p| {
            p.len() > 1024
                || p.contains(&0)
                || p.split(|b| *b == b'/')
                    .any(|c| c.is_empty() || c == b"." || c == b"..")
        })
    {
        return Err(ForgeError::invalid("swarm_paths", "Invalid task paths"));
    }
    paths.sort();
    paths.dedup();
    Ok(())
}

pub(super) fn paths_overlap(a: &[Vec<u8>], b: &[Vec<u8>]) -> bool {
    a.iter().any(|a| {
        b.iter().any(|b| {
            a == b
                || a.strip_prefix(b.as_slice())
                    .is_some_and(|s| s.starts_with(b"/"))
                || b.strip_prefix(a.as_slice())
                    .is_some_and(|s| s.starts_with(b"/"))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claimed_paths_compare_whole_components() {
        assert!(paths_overlap(&[b"src".to_vec()], &[b"src/lib.rs".to_vec()]));
        assert!(!paths_overlap(
            &[b"src".to_vec()],
            &[b"src2/lib.rs".to_vec()]
        ));
    }

    #[test]
    fn terminal_states_do_not_reopen() {
        assert!(transition(SwarmState::Stopped, SwarmState::Working).is_err());
        assert!(transition(SwarmState::Finished, SwarmState::Working).is_err());
        assert!(transition(SwarmState::Paused, SwarmState::Working).is_ok());
    }

    #[test]
    fn unlimited_budget_survives_aggregation() {
        let a = AgentRunBudget {
            max_usd_micros: 10,
            max_changesets: 5,
            ..Default::default()
        };
        let b = AgentRunBudget {
            max_usd_micros: 20,
            ..Default::default()
        };
        let sum = add_budget(a, b);
        assert_eq!(sum.max_usd_micros, 30);
        assert_eq!(sum.max_changesets, 0);
    }
}
