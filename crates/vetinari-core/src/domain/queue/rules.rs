//! The tunable rules that decide when an item comes back.

use time::Duration;

/// How the queue resurfaces items. The defaults are the ones settled with
/// the attention-lifecycle prototype.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueRules {
    /// How long an item is parked when the developer takes something else
    /// while working on it. Zero puts it straight back in the inbox.
    pub switched_away_park: Duration,
    /// Close items that go away at their source, unless they are being
    /// worked on.
    pub close_when_gone: bool,
    /// Reopen finished items that change in a way that needs the developer.
    pub reopen_finished: bool,
    /// Let an urgent change cut a timed park short.
    pub urgent_breaks_snooze: bool,
}

impl Default for QueueRules {
    fn default() -> Self {
        Self {
            switched_away_park: Duration::minutes(10),
            close_when_gone: true,
            reopen_finished: true,
            urgent_breaks_snooze: true,
        }
    }
}
