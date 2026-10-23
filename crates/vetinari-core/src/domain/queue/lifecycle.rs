//! What the developer has decided about an item, as opposed to what its
//! source says about it.

use time::OffsetDateTime;

/// Where a held item stands. The item being worked on is not held: the
/// queue keeps it apart, so there can only ever be one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lifecycle {
    /// Waiting to be picked up.
    Inbox,
    /// Out of the way until something brings it back.
    Parked {
        /// What brings it back.
        until: ParkUntil,
        /// How it came to be parked.
        reason: ParkReason,
    },
    /// Nothing more is wanted from it.
    Done {
        /// How it was closed.
        closure: Closure,
        /// When.
        at: OffsetDateTime,
    },
}

/// What ends a park.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkUntil {
    /// A moment.
    Time(OffsetDateTime),
    /// The source reporting a change that needs the developer.
    Changed,
}

/// How an item came to be parked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkReason {
    /// The developer parked it.
    Chosen,
    /// The developer took something else while working on it.
    SwitchedAway {
        /// When they switched.
        at: OffsetDateTime,
    },
}

/// How an item was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Closure {
    /// The developer finished it; it reopens if it needs them again.
    Finished,
    /// The developer silenced it; later changes are ignored.
    Silenced,
    /// It went away at its source.
    Upstream,
}

/// Why an item came back to the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Rule {
    /// The developer switched away from it mid-way.
    SwitchedAway,
    /// A timed park ran out.
    SnoozeEnded,
    /// It changed while parked until it changed.
    Changed,
    /// An urgent change cut a timed park short.
    UrgentChange,
    /// It needed the developer again after they finished it.
    Reopened,
}

/// A return to the queue, kept on the item until the developer acts on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resurfaced {
    /// The rule that brought it back.
    pub rule: Rule,
    /// When.
    pub at: OffsetDateTime,
}
