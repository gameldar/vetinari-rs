//! The keymap: a per-mode trie from key sequences to [`Action`]s.
//!
//! Storing bindings as a trie is what makes Doom-style prefixes work. A
//! partial sequence resolves to the set of keys that could come next, which is
//! exactly what the which-key popup needs, and a sequence can never be both an
//! action and a prefix.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::action::Action;
use super::keys::{KeyChord, KeySequence};

/// The editing mode a keypress is interpreted in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Vim-style normal mode: keys run commands.
    Normal,
    /// Insert mode: keys are text, except for the few bindings that escape it.
    Insert,
}

impl Mode {
    /// The name used in configuration files.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Mode::Normal => "normal",
            Mode::Insert => "insert",
        }
    }
}

/// Why a binding could not be added.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BindError {
    /// The new sequence is a strict prefix of a binding that already exists,
    /// so it could never be reached.
    #[error("{sequence} is a prefix of an existing binding")]
    PrefixOfExisting {
        /// The sequence that was rejected.
        sequence: KeySequence,
    },
    /// A shorter binding already fires before this sequence completes.
    #[error("shadowed by the existing binding {existing}")]
    ShadowedBy {
        /// The binding that fires first.
        existing: KeySequence,
    },
}

/// What a run of chords resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// The sequence is complete and runs this action.
    Action(Action),
    /// The sequence is a prefix; these keys may follow.
    Prefix(Vec<Candidate>),
    /// Nothing is bound on this path.
    Unbound,
}

/// One continuation offered by a prefix, as shown in the which-key popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The key that would be pressed next.
    pub chord: KeyChord,
    /// The action it runs, if this key completes a binding.
    pub action: Option<Action>,
    /// The label to show: an action description, or a group name.
    pub label: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Node {
    action: Option<Action>,
    group_label: Option<String>,
    children: BTreeMap<KeyChord, Node>,
}

impl Node {
    fn candidates(&self) -> Vec<Candidate> {
        self.children
            .iter()
            .map(|(chord, child)| Candidate {
                chord: *chord,
                action: child.action,
                label: child.label(),
            })
            .collect()
    }

    fn label(&self) -> String {
        if let Some(action) = self.action {
            return action.description().to_owned();
        }
        self.group_label.clone().unwrap_or_else(|| "+prefix".into())
    }

    fn collect(&self, prefix: &mut Vec<KeyChord>, out: &mut Vec<(KeySequence, Action)>) {
        if let Some(action) = self.action
            && let Ok(sequence) = KeySequence::new(prefix.clone())
        {
            out.push((sequence, action));
        }
        for (chord, child) in &self.children {
            prefix.push(*chord);
            child.collect(prefix, out);
            prefix.pop();
        }
    }
}

/// Key sequences mapped to actions, per mode.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Keymap {
    modes: BTreeMap<Mode, Node>,
}

impl Keymap {
    /// A keymap with nothing bound.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Bind a sequence, replacing any action already bound to exactly it.
    pub fn bind(
        &mut self,
        mode: Mode,
        sequence: KeySequence,
        action: Action,
    ) -> Result<(), BindError> {
        let root = self.modes.entry(mode).or_default();
        let chords = sequence.chords().to_vec();
        let mut node = root;
        for (index, chord) in chords.iter().enumerate() {
            if node.action.is_some() {
                let consumed = chords[..index].to_vec();
                return Err(BindError::ShadowedBy {
                    existing: KeySequence::new(consumed)
                        .expect("a shadowing binding has at least one chord"),
                });
            }
            node = node.children.entry(*chord).or_default();
        }
        if !node.children.is_empty() {
            return Err(BindError::PrefixOfExisting { sequence });
        }
        node.action = Some(action);
        Ok(())
    }

    /// Give a prefix a group label, Doom's `+project` style.
    pub fn label_prefix(&mut self, mode: Mode, sequence: &KeySequence, label: impl Into<String>) {
        let mut node = self.modes.entry(mode).or_default();
        for chord in sequence.chords() {
            node = node.children.entry(*chord).or_default();
        }
        node.group_label = Some(label.into());
    }

    /// Remove a binding. Returns whether anything was removed.
    pub fn unbind(&mut self, mode: Mode, sequence: &KeySequence) -> bool {
        let Some(root) = self.modes.get_mut(&mode) else {
            return false;
        };
        fn remove(node: &mut Node, chords: &[KeyChord]) -> bool {
            match chords.split_first() {
                None => node.action.take().is_some(),
                Some((head, tail)) => {
                    let Some(child) = node.children.get_mut(head) else {
                        return false;
                    };
                    let removed = remove(child, tail);
                    if child.action.is_none()
                        && child.children.is_empty()
                        && child.group_label.is_none()
                    {
                        node.children.remove(head);
                    }
                    removed
                }
            }
        }
        remove(root, sequence.chords())
    }

    /// Resolve a run of chords in a mode.
    #[must_use]
    pub fn lookup(&self, mode: Mode, chords: &[KeyChord]) -> Lookup {
        let Some(root) = self.modes.get(&mode) else {
            return Lookup::Unbound;
        };
        let mut node = root;
        for chord in chords {
            match node.children.get(chord) {
                Some(child) => node = child,
                None => return Lookup::Unbound,
            }
        }
        if let Some(action) = node.action {
            return Lookup::Action(action);
        }
        if node.children.is_empty() {
            return Lookup::Unbound;
        }
        Lookup::Prefix(node.candidates())
    }

    /// Every binding in a mode, sorted by sequence.
    #[must_use]
    pub fn bindings(&self, mode: Mode) -> Vec<(KeySequence, Action)> {
        let mut out = Vec::new();
        if let Some(root) = self.modes.get(&mode) {
            root.collect(&mut Vec::new(), &mut out);
        }
        out.sort();
        out
    }

    /// Find the sequence bound to an action, for footer hints.
    #[must_use]
    pub fn sequence_for(&self, mode: Mode, action: Action) -> Option<KeySequence> {
        self.bindings(mode)
            .into_iter()
            .find(|(_, bound)| *bound == action)
            .map(|(sequence, _)| sequence)
    }

    /// The leader key, inferred as the only chord that starts more than one
    /// multi-chord binding in normal mode.
    #[must_use]
    pub fn leader(&self) -> Option<KeyChord> {
        let root = self.modes.get(&Mode::Normal)?;
        root.children
            .iter()
            .filter(|(_, node)| node.action.is_none() && node.children.len() > 1)
            .max_by_key(|(_, node)| node.children.len())
            .map(|(chord, _)| *chord)
    }

    /// The default Doom Emacs / modal Vim keymap from the PRD.
    ///
    /// # Panics
    ///
    /// Panics if the built-in table contains a conflicting binding, which
    /// would be a bug in this function rather than in configuration.
    #[must_use]
    pub fn doom_defaults() -> Self {
        const NORMAL: &[(&str, Action)] = &[
            ("j", Action::NavDown),
            ("k", Action::NavUp),
            ("<Down>", Action::NavDown),
            ("<Up>", Action::NavUp),
            ("g g", Action::NavTop),
            ("G", Action::NavBottom),
            ("C-d", Action::NavPageDown),
            ("C-u", Action::NavPageUp),
            ("/", Action::FilterStart),
            ("<Esc>", Action::FilterClear),
            ("RET", Action::Select),
            ("r", Action::Refresh),
            ("?", Action::Help),
            ("SPC n", Action::QueueTakeNext),
            ("SPC d t", Action::QueueTake),
            ("SPC d d", Action::QueueFinish),
            ("SPC d x", Action::QueueSilence),
            ("SPC d p h", Action::QueueParkHour),
            ("SPC d p t", Action::QueueParkTomorrow),
            ("SPC d p c", Action::QueueParkChanged),
            ("SPC d w a", Action::QueueWaitAgent),
            ("SPC d w r", Action::QueueWaitReply),
            ("SPC t f", Action::FocusToggle),
            ("SPC f f", Action::FeedbackOpen),
            ("SPC f r", Action::FeedbackSubmitLast),
            ("SPC p r", Action::PrReviewSpawn),
            ("SPC p a", Action::PrApprove),
            ("SPC p c", Action::PrComment),
            ("SPC p p", Action::ProfileCycle),
            ("SPC p l", Action::ProfileMenu),
            ("SPC i u", Action::IssueUpdateStatus),
            ("SPC i a", Action::IssueAssign),
            ("SPC i s", Action::IssueStandupSummary),
            ("SPC c r", Action::ChatReply),
            ("SPC c t", Action::ChatConvertToTask),
            ("SPC m m", Action::MeetingActions),
            ("SPC m p", Action::MeetingPrep),
            ("SPC m s", Action::MeetingSchedule),
            ("SPC a a", Action::AgentMenu),
            ("SPC a u", Action::AgentUnblock),
            ("SPC a c", Action::AgentCancel),
            ("SPC a r", Action::AgentRerun),
            ("SPC a l", Action::AgentLogs),
            ("SPC w w", Action::WorkspaceNavigate),
            ("SPC w s", Action::WorkspaceFocusSpace),
            ("SPC w d", Action::WorkspaceCloseSpace),
            ("SPC w r", Action::WorkspaceRunAgent),
            ("SPC s e", Action::SkillEditor),
            ("SPC s r", Action::SkillRun),
            ("SPC h k", Action::Help),
            ("SPC h a", Action::AuthWizard),
            ("SPC r r", Action::Refresh),
            ("SPC q", Action::Quit),
        ];
        const GROUPS: &[(&str, &str)] = &[
            ("SPC d", "+decide"),
            ("SPC d p", "+park"),
            ("SPC d w", "+wait"),
            ("SPC t", "+toggle"),
            ("SPC f", "+feedback"),
            ("SPC p", "+project/profile"),
            ("SPC i", "+issues"),
            ("SPC c", "+chat"),
            ("SPC m", "+meetings"),
            ("SPC a", "+agents"),
            ("SPC w", "+workspace"),
            ("SPC s", "+skills"),
            ("SPC h", "+help"),
            ("SPC r", "+refresh"),
            ("g", "+goto"),
        ];

        let mut keymap = Self::empty();
        for (spelling, action) in NORMAL {
            let sequence = KeySequence::parse(spelling).expect("built-in sequence parses");
            keymap
                .bind(Mode::Normal, sequence, *action)
                .expect("built-in keymap has no conflicts");
        }
        for (spelling, label) in GROUPS {
            let sequence = KeySequence::parse(spelling).expect("built-in sequence parses");
            keymap.label_prefix(Mode::Normal, &sequence, *label);
        }
        keymap
            .bind(
                Mode::Insert,
                KeySequence::parse("<Esc>").expect("built-in sequence parses"),
                Action::CancelPending,
            )
            .expect("built-in keymap has no conflicts");
        keymap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(raw: &str) -> KeySequence {
        KeySequence::parse(raw).expect("sequence")
    }

    fn chord(raw: &str) -> KeyChord {
        KeyChord::parse(raw).expect("chord")
    }

    #[test]
    fn the_queue_is_driven_from_the_leader() {
        let keymap = Keymap::doom_defaults();
        for (spelling, action) in [
            ("SPC n", Action::QueueTakeNext),
            ("SPC d t", Action::QueueTake),
            ("SPC d d", Action::QueueFinish),
            ("SPC d x", Action::QueueSilence),
            ("SPC d p h", Action::QueueParkHour),
            ("SPC d p t", Action::QueueParkTomorrow),
            ("SPC d p c", Action::QueueParkChanged),
            ("SPC d w a", Action::QueueWaitAgent),
            ("SPC d w r", Action::QueueWaitReply),
            ("SPC t f", Action::FocusToggle),
        ] {
            assert_eq!(
                keymap.lookup(Mode::Normal, seq(spelling).chords()),
                Lookup::Action(action),
                "{spelling}"
            );
        }
        let Lookup::Prefix(candidates) = keymap.lookup(Mode::Normal, seq("SPC").chords()) else {
            panic!("expected a prefix");
        };
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.chord.to_string() == "d" && candidate.label == "+decide")
        );
    }

    #[test]
    fn an_exact_sequence_resolves_to_its_action() {
        let mut keymap = Keymap::empty();
        keymap
            .bind(Mode::Normal, seq("SPC f f"), Action::FeedbackOpen)
            .expect("bind");
        assert_eq!(
            keymap.lookup(Mode::Normal, seq("SPC f f").chords()),
            Lookup::Action(Action::FeedbackOpen)
        );
    }

    #[test]
    fn a_partial_sequence_reports_the_candidates_that_follow() {
        let mut keymap = Keymap::empty();
        keymap
            .bind(Mode::Normal, seq("SPC p r"), Action::PrReviewSpawn)
            .expect("bind");
        keymap
            .bind(Mode::Normal, seq("SPC p p"), Action::ProfileCycle)
            .expect("bind");

        let Lookup::Prefix(candidates) = keymap.lookup(Mode::Normal, seq("SPC p").chords()) else {
            panic!("expected a prefix");
        };
        let shown: Vec<_> = candidates
            .iter()
            .map(|c| (c.chord.to_string(), c.label.clone()))
            .collect();
        assert_eq!(
            shown,
            vec![
                ("p".to_owned(), "Switch context profile".to_owned()),
                ("r".to_owned(), "PR review workflow".to_owned()),
            ]
        );
    }

    #[test]
    fn prefix_groups_can_carry_their_own_label() {
        let mut keymap = Keymap::empty();
        keymap
            .bind(Mode::Normal, seq("SPC p r"), Action::PrReviewSpawn)
            .expect("bind");
        keymap.label_prefix(Mode::Normal, &seq("SPC p"), "+project");

        let Lookup::Prefix(candidates) = keymap.lookup(Mode::Normal, seq("SPC").chords()) else {
            panic!("expected a prefix");
        };
        assert_eq!(candidates[0].label, "+project");
        assert_eq!(candidates[0].action, None);
    }

    #[test]
    fn an_unbound_sequence_resolves_to_nothing() {
        let keymap = Keymap::empty();
        assert_eq!(
            keymap.lookup(Mode::Normal, seq("SPC z").chords()),
            Lookup::Unbound
        );
    }

    #[test]
    fn binding_a_prefix_of_an_existing_binding_is_a_conflict() {
        let mut keymap = Keymap::empty();
        keymap
            .bind(Mode::Normal, seq("SPC f f"), Action::FeedbackOpen)
            .expect("bind");
        assert_eq!(
            keymap.bind(Mode::Normal, seq("SPC f"), Action::Help),
            Err(BindError::PrefixOfExisting {
                sequence: seq("SPC f")
            })
        );
    }

    #[test]
    fn binding_past_an_existing_binding_is_a_conflict() {
        let mut keymap = Keymap::empty();
        keymap
            .bind(Mode::Normal, seq("SPC f"), Action::FeedbackOpen)
            .expect("bind");
        assert_eq!(
            keymap.bind(Mode::Normal, seq("SPC f f"), Action::Help),
            Err(BindError::ShadowedBy {
                existing: seq("SPC f")
            })
        );
    }

    #[test]
    fn rebinding_the_same_sequence_replaces_the_action() {
        let mut keymap = Keymap::empty();
        keymap
            .bind(Mode::Normal, seq("SPC q"), Action::Quit)
            .expect("bind");
        keymap
            .bind(Mode::Normal, seq("SPC q"), Action::Help)
            .expect("rebind");
        assert_eq!(
            keymap.lookup(Mode::Normal, seq("SPC q").chords()),
            Lookup::Action(Action::Help)
        );
    }

    #[test]
    fn unbinding_removes_a_sequence_and_prunes_empty_branches() {
        let mut keymap = Keymap::empty();
        keymap
            .bind(Mode::Normal, seq("SPC f f"), Action::FeedbackOpen)
            .expect("bind");
        assert!(keymap.unbind(Mode::Normal, &seq("SPC f f")));
        assert_eq!(
            keymap.lookup(Mode::Normal, seq("SPC").chords()),
            Lookup::Unbound
        );
        assert!(!keymap.unbind(Mode::Normal, &seq("SPC f f")));
    }

    #[test]
    fn modes_keep_separate_bindings() {
        let mut keymap = Keymap::empty();
        keymap
            .bind(Mode::Normal, seq("j"), Action::NavDown)
            .expect("bind");
        assert_eq!(
            keymap.lookup(Mode::Insert, seq("j").chords()),
            Lookup::Unbound
        );
    }

    #[test]
    fn the_leader_key_is_whatever_the_bindings_say_it_is() {
        let keymap = Keymap::doom_defaults();
        assert_eq!(keymap.leader(), Some(chord("SPC")));
    }

    #[test]
    fn doom_defaults_cover_the_prd_bindings() {
        let keymap = Keymap::doom_defaults();
        for (sequence, action) in [
            ("SPC f f", Action::FeedbackOpen),
            ("SPC p r", Action::PrReviewSpawn),
            ("SPC p p", Action::ProfileCycle),
            ("SPC s e", Action::SkillEditor),
            ("SPC a a", Action::AgentMenu),
            ("SPC w w", Action::WorkspaceNavigate),
            ("SPC m m", Action::MeetingActions),
            ("SPC i u", Action::IssueUpdateStatus),
            ("SPC c r", Action::ChatReply),
            ("SPC m p", Action::MeetingPrep),
            ("SPC w s", Action::WorkspaceFocusSpace),
            ("SPC a u", Action::AgentUnblock),
            ("SPC q", Action::Quit),
        ] {
            assert_eq!(
                keymap.lookup(Mode::Normal, seq(sequence).chords()),
                Lookup::Action(action),
                "{sequence} should run {}",
                action.name()
            );
        }
    }

    #[test]
    fn doom_defaults_cover_modal_vim_navigation() {
        let keymap = Keymap::doom_defaults();
        for (sequence, action) in [
            ("j", Action::NavDown),
            ("k", Action::NavUp),
            ("g g", Action::NavTop),
            ("G", Action::NavBottom),
            ("/", Action::FilterStart),
            ("RET", Action::Select),
        ] {
            assert_eq!(
                keymap.lookup(Mode::Normal, seq(sequence).chords()),
                Lookup::Action(action),
                "{sequence} should run {}",
                action.name()
            );
        }
    }

    #[test]
    fn doom_defaults_let_escape_leave_insert_mode() {
        let keymap = Keymap::doom_defaults();
        assert_eq!(
            keymap.lookup(Mode::Insert, seq("<Esc>").chords()),
            Lookup::Action(Action::CancelPending)
        );
    }

    #[test]
    fn every_default_binding_is_reachable_and_listed() {
        let keymap = Keymap::doom_defaults();
        let listed = keymap.bindings(Mode::Normal);
        assert!(listed.len() > 20, "expected a full default keymap");
        for (sequence, action) in &listed {
            assert_eq!(
                keymap.lookup(Mode::Normal, sequence.chords()),
                Lookup::Action(*action)
            );
        }
    }
}
