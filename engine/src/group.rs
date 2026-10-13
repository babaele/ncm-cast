//! Multi-device synchronised playback.
//!
//! Two mechanisms, in order of preference:
//!
//! 1. **Native** — the protocol groups devices itself and keeps them in
//!    lock-step. Only HEOS does this today (`group/set_group`). When a group is
//!    native the engine does nothing else.
//! 2. **Software** — for everything else: all members are started against a
//!    common wall clock and then nudged back into line whenever they drift.
//!    It never reaches sample accuracy across brands, but it stays well inside
//!    what a listener perceives as "the same room".

use crate::model::DeviceId;

/// Below this, drift is inaudible in practice and correcting would cause more
/// stutter than it removes.
pub const DRIFT_TOLERANCE_MS: u64 = 250;
/// Beyond this, seeking is pointless — the member is on a different track or
/// its buffer is gone, so it has to be restarted.
pub const HARD_RESYNC_MS: u64 = 3_000;
/// How often drift is measured.
pub const CORRECTION_INTERVAL_MS: u64 = 5_000;

/// What to do with one member that has drifted from the leader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Correction {
    /// Close enough — leave it alone.
    None,
    /// Seek to this position (leader position plus a small lead, so the member
    /// lands slightly ahead rather than chasing forever).
    Seek(u64),
    /// Reload and start again from the leader's position.
    Restart,
}

/// A set of devices that should play together.
#[derive(Clone, Debug, Default)]
pub struct Group {
    pub members: Vec<DeviceId>,
    /// The member every other one is compared against.
    pub leader: DeviceId,
    /// True when a protocol grouped them natively and we must not interfere.
    pub native: bool,
}

impl Group {
    #[must_use]
    pub fn new(members: Vec<DeviceId>) -> Self {
        let leader = members.first().cloned().unwrap_or_default();
        Self { members, leader, native: false }
    }

    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.members.iter().any(|m| m == id)
    }

    /// Everyone except the leader — the ones that get corrected.
    #[must_use]
    pub fn followers(&self) -> Vec<&DeviceId> {
        self.members.iter().filter(|m| *m != &self.leader).collect()
    }
}

/// Decide what a follower needs, given both positions.
#[must_use]
pub fn correction_between(leader_ms: u64, member_ms: u64) -> Correction {
    let drift = member_ms as i64 - leader_ms as i64;
    if drift.unsigned_abs() <= DRIFT_TOLERANCE_MS {
        return Correction::None;
    }
    if drift.unsigned_abs() > HARD_RESYNC_MS {
        // Seeking will not save it — the member has lost the stream.
        return Correction::Restart;
    }
    // Aim slightly ahead of the leader so the member does not immediately fall
    // behind again while the seek completes.
    Correction::Seek(leader_ms + LEAD_MS)
}

/// How far ahead of the leader a corrected member is aimed.
pub const LEAD_MS: u64 = 120;
