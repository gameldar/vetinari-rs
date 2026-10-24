//! The attention queue: what the developer has decided about each item, kept
//! across refreshes, and the rules that bring items back.
//!
//! Sources only report facts. The queue reconciles each source's snapshot
//! against what it already holds, so "this changed", "this went away" and
//! "you walked away from this" can be noticed at all. Its state is plain
//! owned data, so it can be stored and restored without touching the rules.

pub mod lifecycle;
pub mod record;
pub mod rules;
pub mod view;

use std::collections::{BTreeMap, BTreeSet};

use time::OffsetDateTime;

use self::lifecycle::{Closure, Lifecycle, ParkReason, ParkUntil, Resurfaced, Rule, WaitingOn};
use self::record::RecordEntry;
use self::rules::QueueRules;
use super::attention::{AttentionItem, ItemState, SourceCategory};
use super::ids::{ItemId, SourceId};

/// Whether an item's source still reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upstream {
    /// The last snapshot from its source included it.
    Present,
    /// Its source stopped reporting it.
    Gone,
}

/// An item and the queue's bookkeeping for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    item: AttentionItem,
    upstream: Upstream,
    returned: Option<Resurfaced>,
    queued_at: OffsetDateTime,
    held_by_focus: bool,
    agent: Option<ItemId>,
}

impl Entry {
    const fn new(item: AttentionItem, now: OffsetDateTime) -> Self {
        Self {
            item,
            upstream: Upstream::Present,
            returned: None,
            queued_at: now,
            held_by_focus: false,
            agent: None,
        }
    }

    /// Take a fresh report of the item, saying whether it changed. A report
    /// after the item went away is always a change.
    fn refresh(&mut self, item: AttentionItem) -> bool {
        let changed =
            self.upstream == Upstream::Gone || fingerprint(&self.item) != fingerprint(&item);
        self.item = item;
        self.upstream = Upstream::Present;
        changed
    }

    /// The item, as its source last reported it.
    #[must_use]
    pub const fn item(&self) -> &AttentionItem {
        &self.item
    }

    /// Whether its source still reports it.
    #[must_use]
    pub const fn upstream(&self) -> Upstream {
        self.upstream
    }

    /// Why it came back, until the developer acts on it.
    #[must_use]
    pub const fn returned(&self) -> Option<Resurfaced> {
        self.returned
    }

    /// The agent item working on it, once it has been handed to one.
    #[must_use]
    pub const fn agent(&self) -> Option<&ItemId> {
        self.agent.as_ref()
    }

    /// When it last joined the inbox.
    #[must_use]
    pub const fn queued_at(&self) -> OffsetDateTime {
        self.queued_at
    }
}

/// Where an item stands in the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position<'a> {
    /// The developer is working on it.
    Active {
        /// Since when.
        since: OffsetDateTime,
    },
    /// Anywhere else.
    Held(&'a Lifecycle),
}

/// Something the queue did, for the log and for anything watching it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// The item it happened to.
    pub item: ItemId,
    /// What happened.
    pub happened: Happened,
}

/// What happened to an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Happened {
    /// Its source reported it for the first time.
    Arrived,
    /// It changed, and where it stands did not.
    Updated,
    /// It changed while parked, and the park holds.
    StayedParked,
    /// It changed after being silenced.
    IgnoredSilenced,
    /// It changed after being finished, and stays closed.
    LeftClosed,
    /// A rule brought it back to the queue.
    Resurfaced(Rule),
    /// It went away at its source while being worked on.
    GoneWhileActive,
    /// It went away at its source and was closed.
    ClosedUpstream,
    /// It went away at its source and was left where it was.
    KeptAfterGone,
    /// The developer took it.
    Taken,
    /// The developer switched away from it to something else.
    SwitchedAway,
    /// The developer parked it.
    Parked,
    /// The developer finished it.
    Finished,
    /// The developer silenced it.
    Silenced,
    /// The developer is waiting on a reply to it.
    WaitingOnReply,
    /// It arrived during focus time and is held back until it ends.
    HeldForFocus,
    /// The developer handed it to an agent and is waiting on it.
    HandedToAgent,
    /// The developer is waiting on the agent already working on it.
    WaitingOnAgent,
    /// Its agent needs the developer, who is already working on it.
    AgentNeedsYouWhileActive,
    /// Its agent went away; it is no longer linked to one.
    AgentClosed,
    /// An agent closed along with the item it was working on.
    ClosedWithItem,
}

impl Happened {
    /// Whether it changed where the item stands, as opposed to only what its
    /// source says about it.
    #[must_use]
    pub const fn moves_it(self) -> bool {
        !matches!(
            self,
            Self::Arrived
                | Self::Updated
                | Self::StayedParked
                | Self::IgnoredSilenced
                | Self::LeftClosed
                | Self::KeptAfterGone
                | Self::ClosedWithItem
        )
    }
}

/// How long to park an item for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkFor {
    /// Until a moment.
    Until(OffsetDateTime),
    /// Until its source reports a change that needs the developer.
    Changed,
}

/// A choice the developer makes about an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Work on it now.
    Take(ItemId),
    /// Mark it finished.
    Finish(ItemId),
    /// Close it and ignore its later changes.
    Silence(ItemId),
    /// Put it out of the way.
    Park(ItemId, ParkFor),
    /// Wait on someone's reply to it.
    WaitOnReply(ItemId),
    /// Hand it to an agent, identified by the agent's own item, and wait.
    Delegate {
        /// The item handed over.
        item: ItemId,
        /// The agent's item, which may not have been reported yet.
        agent: ItemId,
    },
    /// Wait on the agent it was already handed to.
    WaitOnAgent(ItemId),
    /// Hold back the muted categories until a moment.
    StartFocus {
        /// When focus time ends.
        until: OffsetDateTime,
    },
    /// End focus time now.
    EndFocus,
}

/// Why a decision could not be made.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// The queue holds no such item.
    #[error("there is no such item")]
    Unknown,
    /// The item is already being worked on.
    #[error("you are already on this")]
    AlreadyActive,
    /// The item is already closed.
    #[error("that is already closed")]
    AlreadyDone,
    /// The item has not been handed to an agent.
    #[error("no agent is working on that")]
    NoAgent,
    /// Focus time has already started.
    #[error("focus time has already started")]
    AlreadyFocused,
    /// Focus time has not started.
    #[error("focus time has not started")]
    NotFocused,
}

/// A stretch of time when some categories are held back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Focus {
    /// When it ends.
    pub until: OffsetDateTime,
    /// What it holds back.
    pub mutes: BTreeSet<SourceCategory>,
}

/// What an agent's own report means for the item it is working on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentSignal {
    NeedsYou,
    Closed,
}

impl AgentSignal {
    fn from_report(entry: &Entry) -> Option<Self> {
        (entry.item.category() == SourceCategory::AgentState && entry.item.state().needs_you())
            .then_some(Self::NeedsYou)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Active {
    entry: Entry,
    since: OffsetDateTime,
}

/// Every item the developer has been shown, and what they decided about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queue {
    rules: QueueRules,
    held: BTreeMap<ItemId, (Entry, Lifecycle)>,
    active: Option<Active>,
    focus: Option<Focus>,
    record: Vec<RecordEntry>,
}

impl Queue {
    /// An empty queue.
    #[must_use]
    pub const fn new(rules: QueueRules) -> Self {
        Self {
            rules,
            held: BTreeMap::new(),
            active: None,
            focus: None,
            record: Vec::new(),
        }
    }

    /// The rules in force.
    #[must_use]
    pub const fn rules(&self) -> &QueueRules {
        &self.rules
    }

    /// Focus time, while it lasts.
    #[must_use]
    pub const fn focus(&self) -> Option<&Focus> {
        self.focus.as_ref()
    }

    /// Bring back whatever time has caught up with.
    pub fn tick(&mut self, now: OffsetDateTime) -> Vec<Note> {
        let mut notes = Vec::new();
        for (id, (entry, lifecycle)) in &mut self.held {
            let rule = match lifecycle {
                Lifecycle::Parked {
                    until: ParkUntil::Time(at),
                    reason,
                } if *at <= now => Some(match reason {
                    ParkReason::SwitchedAway { .. } => Rule::SwitchedAway,
                    ParkReason::Chosen => Rule::SnoozeEnded,
                }),
                Lifecycle::Waiting(WaitingOn::Reply { follow_up_at }) if *follow_up_at <= now => {
                    Some(Rule::FollowUpDue)
                }
                _ => None,
            };
            if let Some(rule) = rule {
                notes.push(Note::new(
                    id.clone(),
                    resurface(now, entry, lifecycle, rule),
                ));
            }
        }
        if self.focus.as_ref().is_some_and(|focus| focus.until <= now) {
            notes.extend(self.end_focus(now));
        }
        self.keep_record(now, &notes);
        notes
    }

    /// Where an item stands.
    #[must_use]
    pub fn position(&self, id: &ItemId) -> Option<Position<'_>> {
        if let Some(active) = self.active_on(id) {
            return Some(Position::Active {
                since: active.since,
            });
        }
        self.held
            .get(id)
            .map(|(_, lifecycle)| Position::Held(lifecycle))
    }

    /// An item and its bookkeeping.
    #[must_use]
    pub fn entry(&self, id: &ItemId) -> Option<&Entry> {
        if let Some(active) = self.active_on(id) {
            return Some(&active.entry);
        }
        self.held.get(id).map(|(entry, _)| entry)
    }

    fn active_on(&self, id: &ItemId) -> Option<&Active> {
        self.active
            .as_ref()
            .filter(|active| active.entry.item.id() == id)
    }

    /// Reconcile one source's complete snapshot. Only call this for a source
    /// that was polled successfully: anything it no longer reports is
    /// treated as gone.
    pub fn observe(
        &mut self,
        now: OffsetDateTime,
        source: &SourceId,
        items: Vec<AttentionItem>,
    ) -> Vec<Note> {
        let mut notes = Vec::new();
        let mut seen = BTreeSet::new();
        let mut signals = Vec::new();

        for item in items {
            let id = item.id().clone();
            seen.insert(id.clone());
            if let Some(active) = self
                .active
                .as_mut()
                .filter(|active| active.entry.item.id() == &id)
            {
                if active.entry.refresh(item) {
                    signals
                        .extend(AgentSignal::from_report(&active.entry).map(|s| (id.clone(), s)));
                    notes.push(Note::new(id, Happened::Updated));
                }
                continue;
            }
            match self.held.get_mut(&id) {
                None => {
                    let mut entry = Entry::new(item, now);
                    entry.held_by_focus = mutes(self.focus.as_ref(), &entry);
                    let happened = if entry.held_by_focus {
                        Happened::HeldForFocus
                    } else {
                        Happened::Arrived
                    };
                    signals.extend(AgentSignal::from_report(&entry).map(|s| (id.clone(), s)));
                    self.held.insert(id.clone(), (entry, Lifecycle::Inbox));
                    notes.push(Note::new(id, happened));
                }
                Some((entry, lifecycle)) => {
                    if entry.refresh(item) {
                        entry.held_by_focus |= mutes(self.focus.as_ref(), entry);
                        let happened = changed(&self.rules, now, entry, lifecycle);
                        signals.extend(AgentSignal::from_report(entry).map(|s| (id.clone(), s)));
                        notes.push(Note::new(id, happened));
                    }
                }
            }
        }

        let vanished = |entry: &Entry| {
            entry.item.source() == source
                && entry.upstream == Upstream::Present
                && !seen.contains(entry.item.id())
        };
        if let Some(active) = self
            .active
            .as_mut()
            .filter(|active| vanished(&active.entry))
        {
            active.entry.upstream = Upstream::Gone;
            notes.push(Note::new(
                active.entry.item.id().clone(),
                Happened::GoneWhileActive,
            ));
        }
        for (id, (entry, lifecycle)) in &mut self.held {
            if vanished(entry) {
                entry.upstream = Upstream::Gone;
                if entry.item.category() == SourceCategory::AgentState {
                    signals.push((id.clone(), AgentSignal::Closed));
                }
                if let Some(happened) = gone(&self.rules, now, entry, lifecycle) {
                    notes.push(Note::new(id.clone(), happened));
                }
            }
        }
        for (agent, signal) in signals {
            notes.extend(self.agent_signal(now, &agent, signal));
        }
        self.keep_record(now, &notes);
        notes
    }

    /// Pass an agent's news on to the items linked to it. A folded agent is
    /// not shown on its own, so its item has to come back in its place.
    fn agent_signal(
        &mut self,
        now: OffsetDateTime,
        agent: &ItemId,
        signal: AgentSignal,
    ) -> Vec<Note> {
        let mut notes = Vec::new();
        if let Some(active) = self
            .active
            .as_mut()
            .filter(|active| active.entry.agent.as_ref() == Some(agent))
        {
            let happened = match signal {
                AgentSignal::NeedsYou => Happened::AgentNeedsYouWhileActive,
                AgentSignal::Closed => {
                    active.entry.agent = None;
                    Happened::AgentClosed
                }
            };
            notes.push(Note::new(active.entry.item.id().clone(), happened));
        }
        for (id, (entry, lifecycle)) in &mut self.held {
            if entry.agent.as_ref() != Some(agent) || matches!(lifecycle, Lifecycle::Done { .. }) {
                continue;
            }
            let waiting = *lifecycle == Lifecycle::Waiting(WaitingOn::Agent);
            let happened = match signal {
                AgentSignal::NeedsYou if waiting || self.rules.fold_agents => {
                    resurface(now, entry, lifecycle, Rule::AgentNeedsYou)
                }
                AgentSignal::NeedsYou => continue,
                AgentSignal::Closed => {
                    entry.agent = None;
                    if waiting {
                        resurface(now, entry, lifecycle, Rule::AgentClosed)
                    } else {
                        Happened::AgentClosed
                    }
                }
            };
            notes.push(Note::new(id.clone(), happened));
        }
        notes
    }

    /// Apply a decision.
    pub fn decide(
        &mut self,
        now: OffsetDateTime,
        decision: Decision,
    ) -> Result<Vec<Note>, Refusal> {
        let notes = match decision {
            Decision::Take(id) => self.take(now, id),
            Decision::Finish(id) => self.close(now, id, Closure::Finished, Happened::Finished),
            Decision::Silence(id) => self.close(now, id, Closure::Silenced, Happened::Silenced),
            Decision::Park(id, park_for) => self.park(id, park_for),
            Decision::WaitOnReply(id) => self.wait_on_reply(now, id),
            Decision::Delegate { item, agent } => {
                let (entry, _) = self.open_item(&item)?;
                entry.agent = Some(agent);
                self.wait_on_agent(now, item, Happened::HandedToAgent)
            }
            Decision::WaitOnAgent(id) => {
                if self.entry(&id).ok_or(Refusal::Unknown)?.agent.is_none() {
                    return Err(Refusal::NoAgent);
                }
                self.wait_on_agent(now, id, Happened::WaitingOnAgent)
            }
            Decision::StartFocus { until } => {
                if self.focus.is_some() {
                    return Err(Refusal::AlreadyFocused);
                }
                self.focus = Some(Focus {
                    until,
                    mutes: self.rules.focus_mutes.clone(),
                });
                Ok(Vec::new())
            }
            Decision::EndFocus => {
                if self.focus.is_none() {
                    return Err(Refusal::NotFocused);
                }
                Ok(self.end_focus(now))
            }
        }?;
        self.keep_record(now, &notes);
        Ok(notes)
    }

    /// Wait on the item's agent, bringing it straight back if the agent
    /// already needs the developer.
    fn wait_on_agent(
        &mut self,
        now: OffsetDateTime,
        id: ItemId,
        happened: Happened,
    ) -> Result<Vec<Note>, Refusal> {
        let agent_needs_you = self
            .entry(&id)
            .and_then(|entry| entry.agent.as_ref())
            .and_then(|agent| self.entry(agent))
            .is_some_and(|agent| agent.item.state().needs_you());
        let (entry, lifecycle) = self.open_item(&id)?;
        entry.returned = None;
        *lifecycle = Lifecycle::Waiting(WaitingOn::Agent);
        let mut notes = vec![Note::new(id.clone(), happened)];
        if agent_needs_you {
            notes.push(Note::new(
                id,
                resurface(now, entry, lifecycle, Rule::AgentNeedsYou),
            ));
        }
        Ok(notes)
    }

    fn wait_on_reply(&mut self, now: OffsetDateTime, id: ItemId) -> Result<Vec<Note>, Refusal> {
        let follow_up_at = now + self.rules.follow_up_after;
        let (entry, lifecycle) = self.open_item(&id)?;
        entry.returned = None;
        *lifecycle = Lifecycle::Waiting(WaitingOn::Reply { follow_up_at });
        Ok(vec![Note::new(id, Happened::WaitingOnReply)])
    }

    /// Release what focus time held back. Anything that arrived or changed
    /// during it comes back marked as such, unless a rule already brought
    /// it back with a better reason.
    fn end_focus(&mut self, now: OffsetDateTime) -> Vec<Note> {
        self.focus = None;
        let mut notes = Vec::new();
        for (id, (entry, lifecycle)) in &mut self.held {
            if !std::mem::take(&mut entry.held_by_focus) {
                continue;
            }
            if *lifecycle == Lifecycle::Inbox && entry.returned.is_none() {
                let happened = resurface(now, entry, lifecycle, Rule::ArrivedDuringFocus);
                notes.push(Note::new(id.clone(), happened));
            }
        }
        notes
    }

    fn take(&mut self, now: OffsetDateTime, id: ItemId) -> Result<Vec<Note>, Refusal> {
        if self.active_on(&id).is_some() {
            return Err(Refusal::AlreadyActive);
        }
        let (mut entry, _) = self.held.remove(&id).ok_or(Refusal::Unknown)?;
        let mut notes = Vec::new();
        if let Some(previous) = self.active.take() {
            notes.push(self.switch_away(now, previous));
        }
        entry.returned = None;
        entry.held_by_focus = false;
        self.active = Some(Active { entry, since: now });
        notes.push(Note::new(id, Happened::Taken));
        Ok(notes)
    }

    fn switch_away(&mut self, now: OffsetDateTime, previous: Active) -> Note {
        let mut entry = previous.entry;
        let park = self.rules.switched_away_park;
        let lifecycle = if park.is_zero() {
            entry.queued_at = now;
            Lifecycle::Inbox
        } else {
            Lifecycle::Parked {
                until: ParkUntil::Time(now + park),
                reason: ParkReason::SwitchedAway { at: now },
            }
        };
        let id = entry.item.id().clone();
        self.held.insert(id.clone(), (entry, lifecycle));
        Note::new(id, Happened::SwitchedAway)
    }

    fn close(
        &mut self,
        now: OffsetDateTime,
        id: ItemId,
        closure: Closure,
        happened: Happened,
    ) -> Result<Vec<Note>, Refusal> {
        let (entry, lifecycle) = self.open_item(&id)?;
        entry.returned = None;
        let agent = entry.agent.clone();
        *lifecycle = Lifecycle::Done { closure, at: now };
        let mut notes = vec![Note::new(id.clone(), happened)];
        if happened == Happened::Finished
            && let Some(agent) = agent
            && let Some((agent_entry, agent_lifecycle)) = self.held.get_mut(&agent)
            && *agent_lifecycle == Lifecycle::Inbox
            && agent_entry.item.state() == ItemState::AgentDone
        {
            *agent_lifecycle = Lifecycle::Done {
                closure: Closure::WithItem(id),
                at: now,
            };
            notes.push(Note::new(agent, Happened::ClosedWithItem));
        }
        Ok(notes)
    }

    fn park(&mut self, id: ItemId, park_for: ParkFor) -> Result<Vec<Note>, Refusal> {
        let until = match park_for {
            ParkFor::Until(at) => ParkUntil::Time(at),
            ParkFor::Changed => ParkUntil::Changed,
        };
        let (entry, lifecycle) = self.open_item(&id)?;
        entry.returned = None;
        *lifecycle = Lifecycle::Parked {
            until,
            reason: ParkReason::Chosen,
        };
        Ok(vec![Note::new(id, Happened::Parked)])
    }

    /// An item that is not closed, with the active one moved back into the
    /// held set so the caller can give it a lifecycle.
    fn open_item(&mut self, id: &ItemId) -> Result<&mut (Entry, Lifecycle), Refusal> {
        if self.active_on(id).is_some()
            && let Some(active) = self.active.take()
        {
            self.held
                .insert(id.clone(), (active.entry, Lifecycle::Inbox));
        }
        let held = self.held.get_mut(id).ok_or(Refusal::Unknown)?;
        if matches!(held.1, Lifecycle::Done { .. }) {
            return Err(Refusal::AlreadyDone);
        }
        Ok(held)
    }
}

impl Note {
    const fn new(item: ItemId, happened: Happened) -> Self {
        Self { item, happened }
    }
}

fn mutes(focus: Option<&Focus>, entry: &Entry) -> bool {
    focus.is_some_and(|focus| focus.mutes.contains(&entry.item.category()))
}

/// What counts as a change: a new state, new activity, or a new deadline.
/// Retitling or reworded details do not bring anything back.
fn fingerprint(item: &AttentionItem) -> (ItemState, OffsetDateTime, Option<OffsetDateTime>) {
    (item.state(), item.occurred_at(), item.due_at())
}

fn changed(
    rules: &QueueRules,
    now: OffsetDateTime,
    entry: &mut Entry,
    lifecycle: &mut Lifecycle,
) -> Happened {
    let state = entry.item.state();
    let rule = match lifecycle {
        Lifecycle::Inbox => return Happened::Updated,
        Lifecycle::Parked {
            until: ParkUntil::Changed,
            ..
        } => state.needs_you().then_some(Rule::Changed),
        Lifecycle::Parked {
            until: ParkUntil::Time(_),
            ..
        } => (rules.urgent_breaks_snooze && state.is_urgent()).then_some(Rule::UrgentChange),
        Lifecycle::Waiting(WaitingOn::Reply { .. }) => Some(Rule::ReplyArrived),
        Lifecycle::Waiting(WaitingOn::Agent) => return Happened::Updated,
        Lifecycle::Done {
            closure: Closure::Silenced,
            ..
        } => return Happened::IgnoredSilenced,
        Lifecycle::Done { .. } => {
            return if rules.reopen_finished && state.needs_you() {
                resurface(now, entry, lifecycle, Rule::Reopened)
            } else {
                Happened::LeftClosed
            };
        }
    };
    rule.map_or(Happened::StayedParked, |rule| {
        resurface(now, entry, lifecycle, rule)
    })
}

fn resurface(
    now: OffsetDateTime,
    entry: &mut Entry,
    lifecycle: &mut Lifecycle,
    rule: Rule,
) -> Happened {
    *lifecycle = Lifecycle::Inbox;
    entry.returned = Some(Resurfaced { rule, at: now });
    entry.queued_at = now;
    Happened::Resurfaced(rule)
}

fn gone(
    rules: &QueueRules,
    now: OffsetDateTime,
    entry: &mut Entry,
    lifecycle: &mut Lifecycle,
) -> Option<Happened> {
    if matches!(lifecycle, Lifecycle::Done { .. }) {
        return None;
    }
    if !rules.close_when_gone {
        return Some(Happened::KeptAfterGone);
    }
    entry.returned = None;
    *lifecycle = Lifecycle::Done {
        closure: Closure::Upstream,
        at: now,
    };
    Some(Happened::ClosedUpstream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attention::SourceCategory;
    use crate::domain::text::NonEmptyText;
    use time::Duration;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-10-22 09:00 UTC);

    fn github() -> SourceId {
        SourceId::parse("github").expect("slug")
    }

    fn jira() -> SourceId {
        SourceId::parse("jira").expect("slug")
    }

    fn report(source: &SourceId, native: &str, state: ItemState) -> AttentionItem {
        reported_at(source, native, state, NOW - Duration::hours(1))
    }

    fn reported_at(
        source: &SourceId,
        native: &str,
        state: ItemState,
        occurred_at: OffsetDateTime,
    ) -> AttentionItem {
        AttentionItem::builder(
            source,
            native,
            SourceCategory::CodeReview,
            NonEmptyText::parse(&format!("item {native}")).expect("title"),
        )
        .state(state)
        .occurred_at(occurred_at)
        .build()
    }

    fn id(source: &SourceId, native: &str) -> ItemId {
        ItemId::from_parts(source, native)
    }

    fn note(item: &ItemId, happened: Happened) -> Note {
        Note {
            item: item.clone(),
            happened,
        }
    }

    fn queue_with(items: &[(&str, ItemState)]) -> Queue {
        let mut queue = Queue::new(QueueRules::default());
        queue.observe(
            NOW,
            &github(),
            items
                .iter()
                .map(|(native, state)| report(&github(), native, *state))
                .collect(),
        );
        queue
    }

    fn held(queue: &Queue, item: &ItemId) -> Lifecycle {
        match queue.position(item) {
            Some(Position::Held(lifecycle)) => lifecycle.clone(),
            other => panic!("{item} is not held: {other:?}"),
        }
    }

    fn decide(queue: &mut Queue, at: OffsetDateTime, decision: Decision) -> Vec<Note> {
        queue.decide(at, decision).expect("decision accepted")
    }

    #[test]
    fn a_new_item_arrives_in_the_inbox() {
        let mut queue = Queue::new(QueueRules::default());
        let notes = queue.observe(
            NOW,
            &github(),
            vec![report(&github(), "41", ItemState::NeedsReview)],
        );

        let pr = id(&github(), "41");
        assert_eq!(notes, vec![note(&pr, Happened::Arrived)]);
        assert_eq!(held(&queue, &pr), Lifecycle::Inbox);
        let entry = queue.entry(&pr).expect("entry");
        assert_eq!(entry.upstream(), Upstream::Present);
        assert_eq!(entry.queued_at(), NOW);
        assert_eq!(entry.returned(), None);
    }

    #[test]
    fn a_snapshot_that_only_retitles_an_item_is_not_a_change() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview)]);
        let retitled = AttentionItem::builder(
            &github(),
            "41",
            SourceCategory::CodeReview,
            NonEmptyText::parse("a better title").expect("title"),
        )
        .state(ItemState::NeedsReview)
        .occurred_at(NOW - Duration::hours(1))
        .build();

        let notes = queue.observe(NOW, &github(), vec![retitled]);

        assert!(notes.is_empty());
        let entry = queue.entry(&id(&github(), "41")).expect("entry");
        assert_eq!(entry.item().title().as_str(), "a better title");
    }

    #[test]
    fn taking_an_item_makes_it_active() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview)]);
        let pr = id(&github(), "41");

        let notes = decide(&mut queue, NOW, Decision::Take(pr.clone()));

        assert_eq!(notes, vec![note(&pr, Happened::Taken)]);
        assert_eq!(queue.position(&pr), Some(Position::Active { since: NOW }));
    }

    #[test]
    fn taking_the_active_item_again_is_refused() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview)]);
        let pr = id(&github(), "41");
        decide(&mut queue, NOW, Decision::Take(pr.clone()));

        assert_eq!(
            queue.decide(NOW, Decision::Take(pr)),
            Err(Refusal::AlreadyActive)
        );
    }

    #[test]
    fn deciding_about_an_unknown_item_is_refused() {
        let mut queue = queue_with(&[]);
        assert_eq!(
            queue.decide(NOW, Decision::Take(id(&github(), "nope"))),
            Err(Refusal::Unknown)
        );
    }

    #[test]
    fn switching_to_another_item_parks_the_first_briefly() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview), ("12", ItemState::CiFailed)]);
        let (review, mine) = (id(&github(), "41"), id(&github(), "12"));
        decide(&mut queue, NOW, Decision::Take(review.clone()));

        let later = NOW + Duration::minutes(3);
        let notes = decide(&mut queue, later, Decision::Take(mine.clone()));

        assert_eq!(
            notes,
            vec![
                note(&review, Happened::SwitchedAway),
                note(&mine, Happened::Taken)
            ]
        );
        assert_eq!(
            held(&queue, &review),
            Lifecycle::Parked {
                until: ParkUntil::Time(later + Duration::minutes(10)),
                reason: ParkReason::SwitchedAway { at: later },
            }
        );
        assert_eq!(
            queue.position(&mine),
            Some(Position::Active { since: later })
        );
    }

    #[test]
    fn with_no_switch_away_park_the_first_item_goes_straight_back() {
        let mut queue = Queue::new(QueueRules {
            switched_away_park: Duration::ZERO,
            ..QueueRules::default()
        });
        queue.observe(
            NOW,
            &github(),
            vec![
                report(&github(), "41", ItemState::NeedsReview),
                report(&github(), "12", ItemState::CiFailed),
            ],
        );
        let review = id(&github(), "41");
        decide(&mut queue, NOW, Decision::Take(review.clone()));

        let later = NOW + Duration::minutes(3);
        decide(&mut queue, later, Decision::Take(id(&github(), "12")));

        assert_eq!(held(&queue, &review), Lifecycle::Inbox);
        assert_eq!(queue.entry(&review).expect("entry").queued_at(), later);
    }

    #[test]
    fn finishing_the_active_item_closes_it() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview)]);
        let pr = id(&github(), "41");
        decide(&mut queue, NOW, Decision::Take(pr.clone()));

        let notes = decide(&mut queue, NOW, Decision::Finish(pr.clone()));

        assert_eq!(notes, vec![note(&pr, Happened::Finished)]);
        assert_eq!(
            held(&queue, &pr),
            Lifecycle::Done {
                closure: Closure::Finished,
                at: NOW
            }
        );
    }

    #[test]
    fn closing_an_item_twice_is_refused() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview)]);
        let pr = id(&github(), "41");
        decide(&mut queue, NOW, Decision::Silence(pr.clone()));

        assert_eq!(
            queue.decide(NOW, Decision::Finish(pr.clone())),
            Err(Refusal::AlreadyDone)
        );
        assert_eq!(
            queue.decide(NOW, Decision::Park(pr, ParkFor::Changed)),
            Err(Refusal::AlreadyDone)
        );
    }

    #[test]
    fn parked_until_changed_holds_through_a_change_that_needs_nothing() {
        let mut queue = queue_with(&[("12", ItemState::CiFailed)]);
        let mine = id(&github(), "12");
        decide(
            &mut queue,
            NOW,
            Decision::Park(mine.clone(), ParkFor::Changed),
        );

        let notes = queue.observe(
            NOW,
            &github(),
            vec![report(&github(), "12", ItemState::CiRunning)],
        );

        assert_eq!(notes, vec![note(&mine, Happened::StayedParked)]);
        assert!(matches!(held(&queue, &mine), Lifecycle::Parked { .. }));
    }

    #[test]
    fn parked_until_changed_comes_back_when_it_needs_you() {
        let mut queue = queue_with(&[("12", ItemState::CiRunning)]);
        let mine = id(&github(), "12");
        decide(
            &mut queue,
            NOW,
            Decision::Park(mine.clone(), ParkFor::Changed),
        );

        let later = NOW + Duration::minutes(20);
        let notes = queue.observe(
            later,
            &github(),
            vec![report(&github(), "12", ItemState::ChangesRequested)],
        );

        assert_eq!(
            notes,
            vec![note(&mine, Happened::Resurfaced(Rule::Changed))]
        );
        assert_eq!(held(&queue, &mine), Lifecycle::Inbox);
        let entry = queue.entry(&mine).expect("entry");
        assert_eq!(
            entry.returned(),
            Some(Resurfaced {
                rule: Rule::Changed,
                at: later
            })
        );
        assert_eq!(entry.queued_at(), later);
    }

    #[test]
    fn new_activity_on_the_same_state_is_a_change() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview)]);
        let review = id(&github(), "41");
        decide(&mut queue, NOW, Decision::Finish(review.clone()));

        let pushed = reported_at(
            &github(),
            "41",
            ItemState::NeedsReview,
            NOW + Duration::minutes(5),
        );
        let notes = queue.observe(NOW + Duration::minutes(5), &github(), vec![pushed]);

        assert_eq!(
            notes,
            vec![note(&review, Happened::Resurfaced(Rule::Reopened))]
        );
    }

    #[test]
    fn a_timed_park_holds_through_a_change_that_is_not_urgent() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview)]);
        let review = id(&github(), "41");
        let until = NOW + Duration::hours(1);
        decide(
            &mut queue,
            NOW,
            Decision::Park(review.clone(), ParkFor::Until(until)),
        );

        let notes = queue.observe(
            NOW,
            &github(),
            vec![report(&github(), "41", ItemState::Unread)],
        );

        assert_eq!(notes, vec![note(&review, Happened::StayedParked)]);
        assert_eq!(
            held(&queue, &review),
            Lifecycle::Parked {
                until: ParkUntil::Time(until),
                reason: ParkReason::Chosen
            }
        );
    }

    #[test]
    fn an_urgent_change_cuts_a_timed_park_short() {
        let mut queue = queue_with(&[("12", ItemState::NeedsReview)]);
        let mine = id(&github(), "12");
        decide(
            &mut queue,
            NOW,
            Decision::Park(mine.clone(), ParkFor::Until(NOW + Duration::hours(1))),
        );

        let notes = queue.observe(
            NOW,
            &github(),
            vec![report(&github(), "12", ItemState::ChangesRequested)],
        );

        assert_eq!(
            notes,
            vec![note(&mine, Happened::Resurfaced(Rule::UrgentChange))]
        );
        assert_eq!(held(&queue, &mine), Lifecycle::Inbox);
    }

    #[test]
    fn an_urgent_change_holds_when_the_rule_is_off() {
        let mut queue = queue_with(&[("12", ItemState::NeedsReview)]);
        queue.rules.urgent_breaks_snooze = false;
        let mine = id(&github(), "12");
        decide(
            &mut queue,
            NOW,
            Decision::Park(mine.clone(), ParkFor::Until(NOW + Duration::hours(1))),
        );

        let notes = queue.observe(
            NOW,
            &github(),
            vec![report(&github(), "12", ItemState::ChangesRequested)],
        );

        assert_eq!(notes, vec![note(&mine, Happened::StayedParked)]);
    }

    #[test]
    fn a_finished_item_reopens_when_it_needs_you_again() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview)]);
        let review = id(&github(), "41");
        decide(&mut queue, NOW, Decision::Finish(review.clone()));

        let notes = queue.observe(
            NOW,
            &github(),
            vec![report(&github(), "41", ItemState::ChangesRequested)],
        );

        assert_eq!(
            notes,
            vec![note(&review, Happened::Resurfaced(Rule::Reopened))]
        );
        assert_eq!(held(&queue, &review), Lifecycle::Inbox);
    }

    #[test]
    fn a_finished_item_stays_closed_when_the_change_needs_nothing() {
        let mut queue = queue_with(&[("12", ItemState::CiFailed)]);
        let mine = id(&github(), "12");
        decide(&mut queue, NOW, Decision::Finish(mine.clone()));

        let notes = queue.observe(
            NOW,
            &github(),
            vec![report(&github(), "12", ItemState::CiRunning)],
        );

        assert_eq!(notes, vec![note(&mine, Happened::LeftClosed)]);
    }

    #[test]
    fn a_finished_item_stays_closed_when_reopening_is_off() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview)]);
        queue.rules.reopen_finished = false;
        let review = id(&github(), "41");
        decide(&mut queue, NOW, Decision::Finish(review.clone()));

        let notes = queue.observe(
            NOW,
            &github(),
            vec![report(&github(), "41", ItemState::ChangesRequested)],
        );

        assert_eq!(notes, vec![note(&review, Happened::LeftClosed)]);
    }

    #[test]
    fn a_silenced_item_ignores_its_changes() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview)]);
        let review = id(&github(), "41");
        let notes = decide(&mut queue, NOW, Decision::Silence(review.clone()));
        assert_eq!(notes, vec![note(&review, Happened::Silenced)]);

        let notes = queue.observe(
            NOW,
            &github(),
            vec![report(&github(), "41", ItemState::ChangesRequested)],
        );

        assert_eq!(notes, vec![note(&review, Happened::IgnoredSilenced)]);
        assert_eq!(
            held(&queue, &review),
            Lifecycle::Done {
                closure: Closure::Silenced,
                at: NOW
            }
        );
    }

    #[test]
    fn a_change_to_the_active_item_leaves_it_active() {
        let mut queue = queue_with(&[("12", ItemState::CiFailed)]);
        let mine = id(&github(), "12");
        decide(&mut queue, NOW, Decision::Take(mine.clone()));

        let notes = queue.observe(
            NOW,
            &github(),
            vec![report(&github(), "12", ItemState::CiRunning)],
        );

        assert_eq!(notes, vec![note(&mine, Happened::Updated)]);
        assert_eq!(queue.position(&mine), Some(Position::Active { since: NOW }));
        assert_eq!(
            queue.entry(&mine).expect("entry").item().state(),
            ItemState::CiRunning
        );
    }

    #[test]
    fn an_item_gone_upstream_is_closed_for_you() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview)]);
        let review = id(&github(), "41");

        let notes = queue.observe(NOW, &github(), Vec::new());

        assert_eq!(notes, vec![note(&review, Happened::ClosedUpstream)]);
        assert_eq!(
            held(&queue, &review),
            Lifecycle::Done {
                closure: Closure::Upstream,
                at: NOW
            }
        );
        assert_eq!(
            queue.entry(&review).expect("entry").upstream(),
            Upstream::Gone
        );
    }

    #[test]
    fn an_item_gone_upstream_is_kept_when_closing_is_off() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview)]);
        queue.rules.close_when_gone = false;
        let review = id(&github(), "41");

        let notes = queue.observe(NOW, &github(), Vec::new());

        assert_eq!(notes, vec![note(&review, Happened::KeptAfterGone)]);
        assert_eq!(held(&queue, &review), Lifecycle::Inbox);
    }

    #[test]
    fn the_active_item_going_away_upstream_stays_yours() {
        let mut queue = queue_with(&[("12", ItemState::CiFailed)]);
        let mine = id(&github(), "12");
        decide(&mut queue, NOW, Decision::Take(mine.clone()));

        let notes = queue.observe(NOW, &github(), Vec::new());

        assert_eq!(notes, vec![note(&mine, Happened::GoneWhileActive)]);
        assert_eq!(queue.position(&mine), Some(Position::Active { since: NOW }));
        assert_eq!(
            queue.entry(&mine).expect("entry").upstream(),
            Upstream::Gone
        );
    }

    #[test]
    fn an_item_is_reported_gone_only_once() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview)]);
        queue.rules.close_when_gone = false;
        queue.observe(NOW, &github(), Vec::new());

        assert!(queue.observe(NOW, &github(), Vec::new()).is_empty());
    }

    #[test]
    fn an_item_reported_again_after_going_away_has_changed() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview)]);
        queue.rules.close_when_gone = false;
        let review = id(&github(), "41");
        queue.observe(NOW, &github(), Vec::new());

        let notes = queue.observe(
            NOW,
            &github(),
            vec![report(&github(), "41", ItemState::NeedsReview)],
        );

        assert_eq!(notes, vec![note(&review, Happened::Updated)]);
        assert_eq!(
            queue.entry(&review).expect("entry").upstream(),
            Upstream::Present
        );
    }

    #[test]
    fn a_snapshot_only_reconciles_its_own_source() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview)]);

        let notes = queue.observe(
            NOW,
            &jira(),
            vec![report(&jira(), "PLAT-311", ItemState::Assigned)],
        );

        let ticket = id(&jira(), "PLAT-311");
        assert_eq!(notes, vec![note(&ticket, Happened::Arrived)]);
        assert_eq!(held(&queue, &id(&github(), "41")), Lifecycle::Inbox);
    }

    #[test]
    fn taking_an_item_that_came_back_clears_why_it_came_back() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview)]);
        let review = id(&github(), "41");
        decide(&mut queue, NOW, Decision::Finish(review.clone()));
        queue.observe(
            NOW,
            &github(),
            vec![report(&github(), "41", ItemState::ChangesRequested)],
        );

        decide(&mut queue, NOW, Decision::Take(review.clone()));

        assert_eq!(queue.entry(&review).expect("entry").returned(), None);
    }

    #[test]
    fn taking_a_closed_item_reopens_it_by_hand() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview)]);
        let review = id(&github(), "41");
        decide(&mut queue, NOW, Decision::Silence(review.clone()));

        decide(&mut queue, NOW, Decision::Take(review.clone()));

        assert_eq!(
            queue.position(&review),
            Some(Position::Active { since: NOW })
        );
    }

    #[test]
    fn the_queue_tracks_items_seen_by_any_source() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview)]);
        queue.observe(
            NOW,
            &jira(),
            vec![report(&jira(), "PLAT-311", ItemState::Assigned)],
        );
        let ids: BTreeSet<_> = [id(&github(), "41"), id(&jira(), "PLAT-311")].into();
        for item in &ids {
            assert!(queue.entry(item).is_some(), "{item}");
        }
    }

    fn slack() -> SourceId {
        SourceId::parse("slack").expect("slug")
    }

    fn message(native: &str, occurred_at: OffsetDateTime) -> AttentionItem {
        AttentionItem::builder(
            &slack(),
            native,
            SourceCategory::Messaging,
            NonEmptyText::parse(&format!("message {native}")).expect("title"),
        )
        .state(ItemState::Unread)
        .occurred_at(occurred_at)
        .build()
    }

    #[test]
    fn a_timed_park_comes_back_when_it_runs_out() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview)]);
        let review = id(&github(), "41");
        let until = NOW + Duration::hours(1);
        decide(
            &mut queue,
            NOW,
            Decision::Park(review.clone(), ParkFor::Until(until)),
        );

        assert!(queue.tick(until - Duration::minutes(1)).is_empty());
        let notes = queue.tick(until);

        assert_eq!(
            notes,
            vec![note(&review, Happened::Resurfaced(Rule::SnoozeEnded))]
        );
        assert_eq!(held(&queue, &review), Lifecycle::Inbox);
        assert_eq!(queue.entry(&review).expect("entry").queued_at(), until);
    }

    #[test]
    fn switching_away_comes_back_once_the_short_park_runs_out() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview), ("12", ItemState::CiFailed)]);
        let review = id(&github(), "41");
        decide(&mut queue, NOW, Decision::Take(review.clone()));
        decide(&mut queue, NOW, Decision::Take(id(&github(), "12")));

        let notes = queue.tick(NOW + Duration::minutes(10));

        assert_eq!(
            notes,
            vec![note(&review, Happened::Resurfaced(Rule::SwitchedAway))]
        );
    }

    #[test]
    fn time_alone_never_brings_back_an_item_parked_until_it_changes() {
        let mut queue = queue_with(&[("12", ItemState::CiFailed)]);
        decide(
            &mut queue,
            NOW,
            Decision::Park(id(&github(), "12"), ParkFor::Changed),
        );

        assert!(queue.tick(NOW + Duration::days(7)).is_empty());
    }

    #[test]
    fn waiting_on_a_reply_comes_back_when_they_respond() {
        let mut queue = Queue::new(QueueRules::default());
        queue.observe(NOW, &slack(), vec![message("dm-1", NOW)]);
        let dm = id(&slack(), "dm-1");

        let notes = decide(&mut queue, NOW, Decision::WaitOnReply(dm.clone()));
        assert_eq!(notes, vec![note(&dm, Happened::WaitingOnReply)]);
        assert_eq!(
            held(&queue, &dm),
            Lifecycle::Waiting(WaitingOn::Reply {
                follow_up_at: NOW + Duration::hours(2)
            })
        );

        let later = NOW + Duration::minutes(30);
        let notes = queue.observe(later, &slack(), vec![message("dm-1", later)]);

        assert_eq!(
            notes,
            vec![note(&dm, Happened::Resurfaced(Rule::ReplyArrived))]
        );
        assert_eq!(held(&queue, &dm), Lifecycle::Inbox);
    }

    #[test]
    fn waiting_on_a_reply_comes_back_to_follow_up() {
        let mut queue = Queue::new(QueueRules::default());
        queue.observe(NOW, &slack(), vec![message("dm-1", NOW)]);
        let dm = id(&slack(), "dm-1");
        decide(&mut queue, NOW, Decision::WaitOnReply(dm.clone()));

        let notes = queue.tick(NOW + Duration::hours(2));

        assert_eq!(
            notes,
            vec![note(&dm, Happened::Resurfaced(Rule::FollowUpDue))]
        );
    }

    #[test]
    fn waiting_on_a_closed_item_is_refused() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview)]);
        let review = id(&github(), "41");
        decide(&mut queue, NOW, Decision::Finish(review.clone()));

        assert_eq!(
            queue.decide(NOW, Decision::WaitOnReply(review)),
            Err(Refusal::AlreadyDone)
        );
    }

    #[test]
    fn focus_cannot_start_twice_or_end_before_it_starts() {
        let mut queue = queue_with(&[]);
        assert_eq!(
            queue.decide(NOW, Decision::EndFocus),
            Err(Refusal::NotFocused)
        );

        let until = NOW + Duration::hours(1);
        assert_eq!(
            queue.decide(NOW, Decision::StartFocus { until }),
            Ok(Vec::new())
        );
        assert_eq!(
            queue.focus(),
            Some(&Focus {
                until,
                mutes: BTreeSet::from([SourceCategory::Messaging])
            })
        );
        assert_eq!(
            queue.decide(NOW, Decision::StartFocus { until }),
            Err(Refusal::AlreadyFocused)
        );
    }

    #[test]
    fn a_muted_arrival_during_focus_is_held_back() {
        let mut queue = queue_with(&[]);
        decide(
            &mut queue,
            NOW,
            Decision::StartFocus {
                until: NOW + Duration::hours(1),
            },
        );

        let notes = queue.observe(NOW, &slack(), vec![message("dm-1", NOW)]);

        assert_eq!(
            notes,
            vec![note(&id(&slack(), "dm-1"), Happened::HeldForFocus)]
        );
    }

    #[test]
    fn focus_leaves_other_categories_alone() {
        let mut queue = queue_with(&[]);
        decide(
            &mut queue,
            NOW,
            Decision::StartFocus {
                until: NOW + Duration::hours(1),
            },
        );

        let notes = queue.observe(
            NOW,
            &github(),
            vec![report(&github(), "41", ItemState::NeedsReview)],
        );

        assert_eq!(notes, vec![note(&id(&github(), "41"), Happened::Arrived)]);
    }

    #[test]
    fn focus_running_out_brings_back_what_arrived_during_it() {
        let mut queue = queue_with(&[]);
        let until = NOW + Duration::hours(1);
        decide(&mut queue, NOW, Decision::StartFocus { until });
        queue.observe(NOW, &slack(), vec![message("dm-1", NOW)]);
        let dm = id(&slack(), "dm-1");

        let notes = queue.tick(until);

        assert_eq!(
            notes,
            vec![note(&dm, Happened::Resurfaced(Rule::ArrivedDuringFocus))]
        );
        assert_eq!(queue.focus(), None);
        assert_eq!(queue.entry(&dm).expect("entry").queued_at(), until);
    }

    #[test]
    fn ending_focus_early_brings_back_what_arrived_during_it() {
        let mut queue = queue_with(&[]);
        decide(
            &mut queue,
            NOW,
            Decision::StartFocus {
                until: NOW + Duration::hours(1),
            },
        );
        queue.observe(NOW, &slack(), vec![message("dm-1", NOW)]);

        let later = NOW + Duration::minutes(20);
        let notes = decide(&mut queue, later, Decision::EndFocus);

        assert_eq!(
            notes,
            vec![note(
                &id(&slack(), "dm-1"),
                Happened::Resurfaced(Rule::ArrivedDuringFocus)
            )]
        );
        assert_eq!(queue.focus(), None);
    }

    #[test]
    fn a_muted_item_that_changes_during_focus_comes_back_after_it() {
        let mut queue = Queue::new(QueueRules::default());
        queue.observe(NOW, &slack(), vec![message("thread", NOW)]);
        let thread = id(&slack(), "thread");
        decide(
            &mut queue,
            NOW,
            Decision::StartFocus {
                until: NOW + Duration::hours(1),
            },
        );

        let later = NOW + Duration::minutes(5);
        assert_eq!(
            queue.observe(later, &slack(), vec![message("thread", later)]),
            vec![note(&thread, Happened::Updated)]
        );
        let notes = decide(&mut queue, later, Decision::EndFocus);

        assert_eq!(
            notes,
            vec![note(
                &thread,
                Happened::Resurfaced(Rule::ArrivedDuringFocus)
            )]
        );
    }

    #[test]
    fn muted_items_untouched_during_focus_come_back_quietly() {
        let mut queue = Queue::new(QueueRules::default());
        queue.observe(NOW, &slack(), vec![message("dm-1", NOW)]);
        decide(
            &mut queue,
            NOW,
            Decision::StartFocus {
                until: NOW + Duration::hours(1),
            },
        );

        assert!(decide(&mut queue, NOW, Decision::EndFocus).is_empty());
        assert_eq!(
            queue
                .entry(&id(&slack(), "dm-1"))
                .expect("entry")
                .returned(),
            None
        );
    }

    #[test]
    fn a_reason_found_during_focus_outlives_it() {
        let mut queue = Queue::new(QueueRules::default());
        queue.observe(NOW, &slack(), vec![message("dm-1", NOW)]);
        let dm = id(&slack(), "dm-1");
        decide(&mut queue, NOW, Decision::WaitOnReply(dm.clone()));
        decide(
            &mut queue,
            NOW,
            Decision::StartFocus {
                until: NOW + Duration::hours(1),
            },
        );
        let later = NOW + Duration::minutes(5);
        queue.observe(later, &slack(), vec![message("dm-1", later)]);

        assert!(decide(&mut queue, later, Decision::EndFocus).is_empty());
        assert_eq!(
            queue
                .entry(&dm)
                .expect("entry")
                .returned()
                .map(|back| back.rule),
            Some(Rule::ReplyArrived)
        );
    }

    #[test]
    fn an_item_looked_at_during_focus_does_not_come_back_after_it() {
        let mut queue = Queue::new(QueueRules {
            switched_away_park: Duration::ZERO,
            ..QueueRules::default()
        });
        queue.observe(
            NOW,
            &github(),
            vec![report(&github(), "41", ItemState::NeedsReview)],
        );
        decide(
            &mut queue,
            NOW,
            Decision::StartFocus {
                until: NOW + Duration::hours(1),
            },
        );
        queue.observe(NOW, &slack(), vec![message("dm-1", NOW)]);
        decide(&mut queue, NOW, Decision::Take(id(&slack(), "dm-1")));
        decide(&mut queue, NOW, Decision::Take(id(&github(), "41")));

        assert!(decide(&mut queue, NOW, Decision::EndFocus).is_empty());
    }

    fn agents() -> SourceId {
        SourceId::parse("agents").expect("slug")
    }

    fn agent_report(native: &str, state: ItemState, occurred_at: OffsetDateTime) -> AttentionItem {
        AttentionItem::builder(
            &agents(),
            native,
            SourceCategory::AgentState,
            NonEmptyText::parse(&format!("agent {native}")).expect("title"),
        )
        .state(state)
        .occurred_at(occurred_at)
        .build()
    }

    /// A review handed to agent w9, which is reported as running.
    fn delegated() -> (Queue, ItemId, ItemId) {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview)]);
        let review = id(&github(), "41");
        let agent = id(&agents(), "w9");
        queue.observe(
            NOW,
            &agents(),
            vec![agent_report("w9", ItemState::AgentRunning, NOW)],
        );
        decide(
            &mut queue,
            NOW,
            Decision::Delegate {
                item: review.clone(),
                agent: agent.clone(),
            },
        );
        (queue, review, agent)
    }

    fn agent_now(queue: &mut Queue, state: ItemState, minutes: i64) -> Vec<Note> {
        let at = NOW + Duration::minutes(minutes);
        queue.observe(at, &agents(), vec![agent_report("w9", state, at)])
    }

    #[test]
    fn handing_an_item_to_an_agent_waits_on_it() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview)]);
        let review = id(&github(), "41");
        let agent = id(&agents(), "w9");

        let notes = decide(
            &mut queue,
            NOW,
            Decision::Delegate {
                item: review.clone(),
                agent: agent.clone(),
            },
        );

        assert_eq!(notes, vec![note(&review, Happened::HandedToAgent)]);
        assert_eq!(held(&queue, &review), Lifecycle::Waiting(WaitingOn::Agent));
        assert_eq!(queue.entry(&review).expect("entry").agent(), Some(&agent));
    }

    #[test]
    fn the_agent_finishing_brings_its_item_back() {
        let (mut queue, review, agent) = delegated();

        let notes = agent_now(&mut queue, ItemState::AgentDone, 15);

        assert_eq!(
            notes,
            vec![
                note(&agent, Happened::Updated),
                note(&review, Happened::Resurfaced(Rule::AgentNeedsYou)),
            ]
        );
        assert_eq!(held(&queue, &review), Lifecycle::Inbox);
    }

    #[test]
    fn the_agent_stopping_for_input_brings_its_item_back() {
        let (mut queue, review, _) = delegated();

        let notes = agent_now(&mut queue, ItemState::AgentWaiting, 5);

        assert!(notes.contains(&note(&review, Happened::Resurfaced(Rule::AgentNeedsYou))));
    }

    #[test]
    fn an_agent_still_working_changes_nothing_for_its_item() {
        let (mut queue, review, agent) = delegated();

        let notes = agent_now(&mut queue, ItemState::AgentRunning, 5);

        assert_eq!(notes, vec![note(&agent, Happened::Updated)]);
        assert_eq!(held(&queue, &review), Lifecycle::Waiting(WaitingOn::Agent));
    }

    #[test]
    fn the_agent_finishing_while_you_watch_leaves_you_on_it() {
        let (mut queue, review, _) = delegated();
        decide(&mut queue, NOW, Decision::Take(review.clone()));

        let notes = agent_now(&mut queue, ItemState::AgentDone, 5);

        assert!(notes.contains(&note(&review, Happened::AgentNeedsYouWhileActive)));
        assert_eq!(
            queue.position(&review),
            Some(Position::Active { since: NOW })
        );
    }

    #[test]
    fn a_parked_item_comes_back_when_its_folded_agent_needs_you() {
        let (mut queue, review, _) = delegated();
        decide(
            &mut queue,
            NOW,
            Decision::Park(review.clone(), ParkFor::Until(NOW + Duration::hours(2))),
        );

        let notes = agent_now(&mut queue, ItemState::AgentWaiting, 5);

        assert!(notes.contains(&note(&review, Happened::Resurfaced(Rule::AgentNeedsYou))));
    }

    #[test]
    fn a_parked_item_stays_parked_when_agents_are_not_folded() {
        let (mut queue, review, _) = delegated();
        queue.rules.fold_agents = false;
        decide(
            &mut queue,
            NOW,
            Decision::Park(review.clone(), ParkFor::Until(NOW + Duration::hours(2))),
        );

        agent_now(&mut queue, ItemState::AgentWaiting, 5);

        assert!(matches!(held(&queue, &review), Lifecycle::Parked { .. }));
    }

    #[test]
    fn waiting_on_an_agent_needs_one() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview)]);

        assert_eq!(
            queue.decide(NOW, Decision::WaitOnAgent(id(&github(), "41"))),
            Err(Refusal::NoAgent)
        );
    }

    #[test]
    fn waiting_on_an_agent_that_already_finished_comes_straight_back() {
        let (mut queue, review, _) = delegated();
        decide(&mut queue, NOW, Decision::Take(review.clone()));
        agent_now(&mut queue, ItemState::AgentDone, 5);

        let notes = decide(&mut queue, NOW, Decision::WaitOnAgent(review.clone()));

        assert_eq!(
            notes,
            vec![
                note(&review, Happened::WaitingOnAgent),
                note(&review, Happened::Resurfaced(Rule::AgentNeedsYou)),
            ]
        );
    }

    #[test]
    fn waiting_on_an_agent_still_working_waits() {
        let (mut queue, review, _) = delegated();
        decide(&mut queue, NOW, Decision::Take(review.clone()));

        let notes = decide(&mut queue, NOW, Decision::WaitOnAgent(review.clone()));

        assert_eq!(notes, vec![note(&review, Happened::WaitingOnAgent)]);
        assert_eq!(held(&queue, &review), Lifecycle::Waiting(WaitingOn::Agent));
    }

    #[test]
    fn the_agent_going_away_frees_its_item() {
        let (mut queue, review, agent) = delegated();

        let notes = queue.observe(NOW, &agents(), Vec::new());

        assert_eq!(
            notes,
            vec![
                note(&agent, Happened::ClosedUpstream),
                note(&review, Happened::Resurfaced(Rule::AgentClosed)),
            ]
        );
        assert_eq!(queue.entry(&review).expect("entry").agent(), None);
    }

    #[test]
    fn the_agent_going_away_unlinks_an_item_not_waiting_on_it() {
        let (mut queue, review, _) = delegated();
        decide(&mut queue, NOW, Decision::Take(review.clone()));

        let notes = queue.observe(NOW, &agents(), Vec::new());

        assert!(notes.contains(&note(&review, Happened::AgentClosed)));
        assert_eq!(queue.entry(&review).expect("entry").agent(), None);
    }

    #[test]
    fn an_agent_reported_for_the_first_time_already_blocked_brings_its_item_back() {
        let mut queue = queue_with(&[("41", ItemState::NeedsReview)]);
        let review = id(&github(), "41");
        let agent = id(&agents(), "w9");
        decide(
            &mut queue,
            NOW,
            Decision::Delegate {
                item: review.clone(),
                agent: agent.clone(),
            },
        );

        let notes = queue.observe(
            NOW,
            &agents(),
            vec![agent_report("w9", ItemState::AgentWaiting, NOW)],
        );

        assert_eq!(
            notes,
            vec![
                note(&agent, Happened::Arrived),
                note(&review, Happened::Resurfaced(Rule::AgentNeedsYou)),
            ]
        );
    }

    #[test]
    fn finishing_an_item_closes_its_finished_agent_with_it() {
        let (mut queue, review, agent) = delegated();
        agent_now(&mut queue, ItemState::AgentDone, 5);
        decide(&mut queue, NOW, Decision::Take(review.clone()));

        let notes = decide(&mut queue, NOW, Decision::Finish(review.clone()));

        assert_eq!(
            notes,
            vec![
                note(&review, Happened::Finished),
                note(&agent, Happened::ClosedWithItem)
            ]
        );
        assert_eq!(
            held(&queue, &agent),
            Lifecycle::Done {
                closure: Closure::WithItem(review),
                at: NOW
            }
        );
    }

    #[test]
    fn finishing_an_item_leaves_an_agent_still_working_open() {
        let (mut queue, review, agent) = delegated();
        decide(&mut queue, NOW, Decision::Take(review.clone()));

        let notes = decide(&mut queue, NOW, Decision::Finish(review.clone()));

        assert_eq!(notes, vec![note(&review, Happened::Finished)]);
        assert_eq!(held(&queue, &agent), Lifecycle::Inbox);
    }
}
