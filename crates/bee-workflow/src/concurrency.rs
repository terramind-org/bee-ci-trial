//! Concurrency groups: which runs or jobs start, wait, or get cancelled.
//!
//! GitHub's rules ("Control the concurrency of workflows and jobs"):
//!
//! * Group names are case-insensitive (`prod` and `Prod` are one group).
//! * At most one run (or job) in a group is in progress.
//! * With the default `queue: single`, at most one is pending: a newly queued one cancels and
//!   replaces the pending one.
//! * With `queue: max`, up to 100 may be pending, started in arrival order.
//! * With `cancel-in-progress: true`, a newly queued one cancels the one in progress (and any
//!   pending) and starts at once.

/// The pending limit for `queue: max`.
pub const MAX_PENDING: usize = 100;

/// Normalises a group name for comparison.
pub fn group_key(group: &str) -> String {
    group.to_lowercase()
}

/// The state of a member of a group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberState {
    /// In progress.
    Running,
    /// Waiting for the group.
    Pending,
}

/// A run or job already in the group, oldest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// The run or job id.
    pub id: String,
    /// Its state.
    pub state: MemberState,
}

/// What a newcomer asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Request {
    /// `cancel-in-progress`, already evaluated.
    pub cancel_in_progress: bool,
    /// `queue: max`.
    pub queue_max: bool,
}

/// The decision for a newcomer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Admission {
    /// Members to cancel, in the order given.
    pub cancel: Vec<String>,
    /// Whether the newcomer starts now (otherwise it is pending).
    pub start: bool,
}

/// Decides what happens when a run or job joins a group whose current members are `members`.
pub fn admit(members: &[Member], request: Request) -> Admission {
    if request.cancel_in_progress {
        return Admission {
            cancel: members.iter().map(|m| m.id.clone()).collect(),
            start: true,
        };
    }
    let running = members.iter().any(|m| m.state == MemberState::Running);
    let pending: Vec<&Member> = members
        .iter()
        .filter(|m| m.state == MemberState::Pending)
        .collect();
    let cancel: Vec<String> = if request.queue_max {
        // Over the limit, the oldest pending entries give way.
        let excess = (pending.len() + 1).saturating_sub(MAX_PENDING);
        if running {
            pending.iter().take(excess).map(|m| m.id.clone()).collect()
        } else {
            Vec::new()
        }
    } else {
        pending.iter().map(|m| m.id.clone()).collect()
    };
    let start = !running && (request.queue_max && pending.is_empty() || !request.queue_max);
    Admission { cancel, start }
}

/// Which pending member starts when the group frees up: the oldest one.
pub fn next_to_start(members: &[Member]) -> Option<&str> {
    if members.iter().any(|m| m.state == MemberState::Running) {
        return None;
    }
    members
        .iter()
        .find(|m| m.state == MemberState::Pending)
        .map(|m| m.id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: &str, state: MemberState) -> Member {
        Member {
            id: id.into(),
            state,
        }
    }

    #[test]
    fn empty_group_starts_immediately() {
        assert_eq!(
            admit(&[], Request::default()),
            Admission {
                cancel: vec![],
                start: true
            }
        );
    }

    #[test]
    fn single_queue_replaces_the_pending_run() {
        let members = [m("r1", MemberState::Running), m("r2", MemberState::Pending)];
        assert_eq!(
            admit(&members, Request::default()),
            Admission {
                cancel: vec!["r2".into()],
                start: false
            }
        );
        assert_eq!(
            admit(&members[..1], Request::default()),
            Admission {
                cancel: vec![],
                start: false
            }
        );
    }

    #[test]
    fn cancel_in_progress_cancels_everything() {
        let members = [m("r1", MemberState::Running), m("r2", MemberState::Pending)];
        let req = Request {
            cancel_in_progress: true,
            queue_max: false,
        };
        assert_eq!(
            admit(&members, req),
            Admission {
                cancel: vec!["r1".into(), "r2".into()],
                start: true
            }
        );
    }

    #[test]
    fn queue_max_keeps_up_to_100_pending() {
        let req = Request {
            cancel_in_progress: false,
            queue_max: true,
        };
        let mut members = vec![m("run", MemberState::Running)];
        members.extend((0..99).map(|i| m(&format!("p{i}"), MemberState::Pending)));
        assert_eq!(
            admit(&members, req),
            Admission {
                cancel: vec![],
                start: false
            }
        );
        members.push(m("p99", MemberState::Pending));
        assert_eq!(
            admit(&members, req),
            Admission {
                cancel: vec!["p0".into()],
                start: false
            }
        );
        assert_eq!(next_to_start(&members), None);
        assert_eq!(next_to_start(&members[1..]), Some("p0"));
    }

    #[test]
    fn group_names_are_case_insensitive() {
        assert_eq!(group_key("Prod"), group_key("prod"));
    }
}
