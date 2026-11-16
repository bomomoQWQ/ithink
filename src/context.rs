//! Task-local context: an append-only operation log and a separately derived view.
//!
//! Units and pairing identifiers are supplied by the caller. This module neither
//! invokes a model nor edits, summarizes, or truncates content automatically.
//! Replacement ranges use half-open item indices, `[start, end)`.

use crate::api::{ContextItem, ContextLimits, KernelError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextSnapshot {
    pub revision: u64,
    pub used: u64,
    pub threshold: u64,
    pub max_input: u64,
    pub pressure: bool,
    pub items: Vec<ContextItem>,
}

/// An immutable fact about a committed context operation.
///
/// `revision` is the resulting revision; the seed is always revision zero.
/// Replacements and view releases retain the exact original items, so the log
/// preserves source data and can validate deterministic replay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ContextEntry {
    Seed {
        revision: u64,
        items: Vec<ContextItem>,
    },
    Append {
        revision: u64,
        items: Vec<ContextItem>,
    },
    Replace {
        revision: u64,
        start: usize,
        end: usize,
        original: Vec<ContextItem>,
        replacement: Vec<ContextItem>,
    },
    ReleaseView {
        revision: u64,
        original: Vec<ContextItem>,
    },
}

impl ContextEntry {
    pub fn revision(&self) -> u64 {
        match self {
            Self::Seed { revision, .. }
            | Self::Append { revision, .. }
            | Self::Replace { revision, .. }
            | Self::ReleaseView { revision, .. } => *revision,
        }
    }
}

/// A context belongs to one task, with no shared mutable or global state.
///
/// Successful mutations increment the revision once, including empty appends.
/// Validation is completed before either the view or the log is changed.
#[derive(Debug, Clone)]
pub struct Context {
    revision: u64,
    used: u64,
    threshold: u64,
    max_input: u64,
    items: Vec<ContextItem>,
    entries: Vec<ContextEntry>,
}

impl Context {
    pub fn new(limits: ContextLimits, seed: Vec<ContextItem>) -> Result<Self, KernelError> {
        let threshold = limits.threshold()?;
        let max_input = limits.max_input();
        let used = total_units(&seed)?;
        ensure_capacity(used, max_input)?;
        pairing_state(&seed)?;
        let entry = ContextEntry::Seed {
            revision: 0,
            items: seed.clone(),
        };
        Ok(Self {
            revision: 0,
            used,
            threshold,
            max_input,
            items: seed,
            entries: vec![entry],
        })
    }

    pub fn snapshot(&self) -> ContextSnapshot {
        ContextSnapshot {
            revision: self.revision,
            used: self.used,
            threshold: self.threshold,
            max_input: self.max_input,
            pressure: self.used >= self.threshold,
            items: self.items.clone(),
        }
    }

    /// Append exact caller-supplied items. Open calls may remain pending, but
    /// every close must answer one previously opened, not-yet-closed ID.
    pub fn append(
        &mut self,
        expected_revision: u64,
        items: Vec<ContextItem>,
    ) -> Result<ContextSnapshot, KernelError> {
        self.check_revision(expected_revision)?;
        let revision = self.next_revision()?;
        let added = total_units(&items)?;
        let used = self.used.checked_add(added).ok_or_else(unit_overflow)?;
        ensure_capacity(used, self.max_input)?;

        let mut pairing = pairing_state(&self.items)?;
        for item in &items {
            pairing.accept(item)?;
        }

        // No fallible validation remains after this point.
        self.entries.push(ContextEntry::Append {
            revision,
            items: items.clone(),
        });
        self.items.extend(items);
        self.used = used;
        self.revision = revision;
        Ok(self.snapshot())
    }

    /// Replace a complete, balanced span with a strictly smaller span.
    ///
    /// A pinned item cannot be selected, changed, removed, or moved to another
    /// item index. Thus a length-changing replacement before a pinned suffix is
    /// also rejected. Appending after pinned items does not move them.
    ///
    /// Both boundaries must have no pending opens. The replacement must itself
    /// be balanced, and the complete resulting view must remain valid. A
    /// pending open elsewhere in the view therefore cannot be forged or lost.
    pub fn replace(
        &mut self,
        expected_revision: u64,
        start: usize,
        end: usize,
        replacement: Vec<ContextItem>,
    ) -> Result<ContextSnapshot, KernelError> {
        self.check_revision(expected_revision)?;
        let revision = self.next_revision()?;
        if start >= end || end > self.items.len() {
            return Err(KernelError::invalid("invalid context replacement range"));
        }
        let original = &self.items[start..end];
        if original.iter().any(|item| item.pinned) {
            return Err(KernelError::invalid(
                "context replacement includes a pinned item",
            ));
        }
        if replacement.len() != end - start && self.items[end..].iter().any(|item| item.pinned) {
            return Err(KernelError::invalid(
                "context replacement would move a pinned item",
            ));
        }
        ensure_balanced_boundary(&self.items, start)?;
        ensure_balanced_boundary(&self.items, end)?;
        if !pairing_state(&replacement)?.pending.is_empty() {
            return Err(KernelError::invalid(
                "context replacement has unmatched open IDs",
            ));
        }

        let removed = total_units(original)?;
        let added = total_units(&replacement)?;
        if added >= removed {
            return Err(KernelError::invalid(
                "context replacement must strictly reduce used units",
            ));
        }
        let used = self
            .used
            .checked_sub(removed)
            .and_then(|remaining| remaining.checked_add(added))
            .ok_or_else(unit_overflow)?;
        ensure_capacity(used, self.max_input)?;
        let new_len = (self.items.len() - (end - start))
            .checked_add(replacement.len())
            .ok_or_else(|| KernelError::invalid("context item count overflow"))?;
        let mut candidate = Vec::with_capacity(new_len);
        candidate.extend_from_slice(&self.items[..start]);
        candidate.extend_from_slice(&replacement);
        candidate.extend_from_slice(&self.items[end..]);
        let before = pairing_state(&self.items)?;
        let after = pairing_state(&candidate)?;
        if before.pending != after.pending {
            return Err(KernelError::invalid(
                "context replacement changes unmatched open IDs",
            ));
        }

        let entry = ContextEntry::Replace {
            revision,
            start,
            end,
            original: original.to_vec(),
            replacement,
        };
        // Commit the view and its log entry only after every check succeeded.
        self.entries.push(entry);
        self.items = candidate;
        self.used = used;
        self.revision = revision;
        Ok(self.snapshot())
    }

    pub fn history(&self) -> &[ContextEntry] {
        &self.entries
    }

    /// Release the active view when a task ends, retaining all history.
    ///
    /// This is not compaction: it may discard pinned and pending items from
    /// the view, but their exact originals remain in the log. The revision
    /// advances to invalidate outstanding edits. The task owner, rather than
    /// this storage primitive, decides whether any further writes are allowed.
    pub fn release_view(&mut self) -> Result<ContextSnapshot, KernelError> {
        let revision = self.next_revision()?;
        let original = std::mem::take(&mut self.items);
        self.entries
            .push(ContextEntry::ReleaseView { revision, original });
        self.used = 0;
        self.revision = revision;
        Ok(self.snapshot())
    }

    /// Rebuild a context from its full log and reject malformed or tampered
    /// operations. Accepts either a history slice or an owned entry vector.
    /// The caller supplies the limits under which the log is to be replayed.
    pub fn replay(
        limits: ContextLimits,
        entries: impl AsRef<[ContextEntry]>,
    ) -> Result<Self, KernelError> {
        let entries = entries.as_ref();
        let seed = match entries.first() {
            Some(ContextEntry::Seed { revision: 0, items }) => items.clone(),
            _ => {
                return Err(KernelError::invalid(
                    "context history must start with a revision-zero seed",
                ));
            }
        };
        let mut context = Self::new(limits, seed)?;
        for entry in &entries[1..] {
            if entry.revision() != context.next_revision()? {
                return Err(KernelError::invalid(
                    "context history revisions must be consecutive",
                ));
            }
            match entry {
                ContextEntry::Seed { .. } => {
                    return Err(KernelError::invalid(
                        "context history contains more than one seed",
                    ));
                }
                ContextEntry::Append { items, .. } => {
                    context.append(context.revision, items.clone())?;
                }
                ContextEntry::Replace {
                    start,
                    end,
                    original,
                    replacement,
                    ..
                } => {
                    if context.items.get(*start..*end) != Some(original.as_slice()) {
                        return Err(KernelError::invalid(
                            "context history replacement originals do not match",
                        ));
                    }
                    context.replace(context.revision, *start, *end, replacement.clone())?;
                }
                ContextEntry::ReleaseView { original, .. } => {
                    if context.items != *original {
                        return Err(KernelError::invalid(
                            "context history release originals do not match",
                        ));
                    }
                    context.release_view()?;
                }
            }
        }
        Ok(context)
    }

    fn check_revision(&self, expected: u64) -> Result<(), KernelError> {
        if self.revision != expected {
            return Err(KernelError::new(
                "revision_conflict",
                format!(
                    "expected context revision {expected}, current revision is {}",
                    self.revision
                ),
            ));
        }
        Ok(())
    }

    fn next_revision(&self) -> Result<u64, KernelError> {
        self.revision
            .checked_add(1)
            .ok_or_else(|| KernelError::new("revision_exhausted", "context revision exhausted"))
    }
}

fn unit_overflow() -> KernelError {
    KernelError::new("context_capacity_exceeded", "context units overflow u64")
}

fn total_units(items: &[ContextItem]) -> Result<u64, KernelError> {
    items.iter().try_fold(0_u64, |used, item| {
        used.checked_add(item.units).ok_or_else(unit_overflow)
    })
}

fn ensure_capacity(used: u64, max_input: u64) -> Result<(), KernelError> {
    if used > max_input {
        return Err(KernelError::new(
            "context_capacity_exceeded",
            format!("context uses {used} units, input limit is {max_input}"),
        ));
    }
    Ok(())
}

#[derive(Default)]
struct PairingState {
    // `seen` also prevents reusing an ID after its first pair has closed.
    seen: BTreeSet<String>,
    pending: BTreeSet<String>,
}

impl PairingState {
    fn accept(&mut self, item: &ContextItem) -> Result<(), KernelError> {
        // An atomic item may both open and close an ID. Parallel calls may
        // close in any order: IDs are opaque, not a stack or a nesting scheme.
        for id in &item.opens {
            if !self.seen.insert(id.clone()) {
                return Err(KernelError::invalid(format!(
                    "duplicate context open ID: {id}"
                )));
            }
            self.pending.insert(id.clone());
        }
        for id in &item.closes {
            if !self.pending.remove(id) {
                return Err(KernelError::invalid(format!(
                    "duplicate or orphan context close ID: {id}"
                )));
            }
        }
        Ok(())
    }
}

fn pairing_state(items: &[ContextItem]) -> Result<PairingState, KernelError> {
    let mut state = PairingState::default();
    for item in items {
        state.accept(item)?;
    }
    Ok(state)
}

fn ensure_balanced_boundary(items: &[ContextItem], boundary: usize) -> Result<(), KernelError> {
    if !pairing_state(&items[..boundary])?.pending.is_empty() {
        return Err(KernelError::invalid(format!(
            "context replacement boundary {boundary} splits an open/close pair"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn limits() -> ContextLimits {
        ContextLimits {
            capacity: 100,
            output_reserve: 10,
            headroom: 10,
            pressure_bps: 8000,
        }
    }

    fn item(text: &str, units: u64) -> ContextItem {
        ContextItem::text(text, units)
    }

    fn open(id: &str, units: u64) -> ContextItem {
        ContextItem {
            opens: vec![id.to_owned()],
            ..item("call", units)
        }
    }

    fn close(id: &str, units: u64) -> ContextItem {
        ContextItem {
            closes: vec![id.to_owned()],
            ..item("result", units)
        }
    }

    fn pinned(text: &str, units: u64) -> ContextItem {
        ContextItem {
            pinned: true,
            ..item(text, units)
        }
    }

    fn assert_unchanged(context: &Context, snapshot: &ContextSnapshot, log: &[ContextEntry]) {
        assert_eq!(&context.snapshot(), snapshot);
        assert_eq!(context.history(), log);
    }

    #[test]
    fn empty_seed_has_revision_zero_and_validated_limits() {
        let context = Context::new(limits(), vec![]).unwrap();
        assert_eq!(
            context.snapshot(),
            ContextSnapshot {
                revision: 0,
                used: 0,
                threshold: 80,
                max_input: 90,
                pressure: false,
                items: vec![],
            }
        );
        assert_eq!(
            context.history(),
            &[ContextEntry::Seed {
                revision: 0,
                items: vec![]
            }]
        );
        let mut invalid = limits();
        invalid.output_reserve = 101;
        assert!(Context::new(invalid, vec![]).is_err());
        let mut invalid = limits();
        invalid.headroom = 90;
        assert!(Context::new(invalid, vec![]).is_err());
        let mut invalid = limits();
        invalid.pressure_bps = 10_001;
        assert!(Context::new(invalid, vec![]).is_err());
    }

    #[test]
    fn pressure_is_advisory_and_does_not_truncate() {
        let mut context = Context::new(limits(), vec![item("first", 79)]).unwrap();
        assert!(!context.snapshot().pressure);
        let snapshot = context.append(0, vec![item("threshold", 1)]).unwrap();
        assert_eq!(snapshot.used, limits().threshold().unwrap());
        assert!(snapshot.pressure);
        let snapshot = context.append(1, vec![item("near limit", 10)]).unwrap();
        assert_eq!(snapshot.used, 90);
        assert_eq!(snapshot.items.len(), 3);
        assert_eq!(snapshot.items[0], item("first", 79));
    }

    #[test]
    fn capacity_uses_output_reserve_as_a_hard_limit() {
        assert!(Context::new(limits(), vec![item("full", 90)]).is_ok());
        assert!(Context::new(limits(), vec![item("too much", 91)]).is_err());
        let mut context = Context::new(limits(), vec![item("full", 90)]).unwrap();
        let before = context.snapshot();
        let history = context.history().to_vec();
        let error = context.append(0, vec![item("overflow", 1)]).unwrap_err();
        assert_eq!(error.code, "context_capacity_exceeded");
        assert_unchanged(&context, &before, &history);
    }

    #[test]
    fn unit_sums_use_checked_arithmetic() {
        let huge_limits = ContextLimits {
            capacity: u64::MAX,
            output_reserve: 0,
            headroom: 1,
            pressure_bps: 8000,
        };
        assert!(
            Context::new(
                huge_limits.clone(),
                vec![item("max", u64::MAX), item("overflow", 1)],
            )
            .is_err()
        );
        let mut context = Context::new(huge_limits, vec![item("max", u64::MAX)]).unwrap();
        let before = context.snapshot();
        let history = context.history().to_vec();
        assert!(context.append(0, vec![item("overflow", 1)]).is_err());
        assert_unchanged(&context, &before, &history);
        let mut context = Context::new(limits(), vec![item("old", 80)]).unwrap();
        let before = context.snapshot();
        let history = context.history().to_vec();
        assert!(
            context
                .replace(0, 0, 1, vec![item("max", u64::MAX), item("overflow", 1)])
                .is_err()
        );
        assert_unchanged(&context, &before, &history);
    }

    #[test]
    fn stale_revision_never_changes_view_or_log() {
        let mut context = Context::new(limits(), vec![item("seed", 20)]).unwrap();
        context.append(0, vec![item("new", 10)]).unwrap();
        let before = context.snapshot();
        let history = context.history().to_vec();
        let error = context.append(0, vec![item("stale", 1)]).unwrap_err();
        assert_eq!(error.code, "revision_conflict");
        assert_unchanged(&context, &before, &history);
        let error = context
            .replace(0, 0, 1, vec![item("short", 1)])
            .unwrap_err();
        assert_eq!(error.code, "revision_conflict");
        assert_unchanged(&context, &before, &history);
    }

    #[test]
    fn append_may_leave_open_calls_and_later_close_them() {
        let mut context = Context::new(limits(), vec![]).unwrap();
        context
            .append(0, vec![open("call-a", 10), open("call-b", 10)])
            .unwrap();
        // Out-of-order completion is legal; this is not a stack.
        context
            .append(1, vec![close("call-a", 10), close("call-b", 10)])
            .unwrap();
        assert_eq!(context.snapshot().used, 40);
        assert_eq!(context.snapshot().items.len(), 4);
        context.replace(2, 0, 4, vec![item("summary", 5)]).unwrap();
    }

    #[test]
    fn duplicate_and_orphan_closes_are_rejected_atomically() {
        let mut context = Context::new(limits(), vec![open("call-a", 10)]).unwrap();
        let before = context.snapshot();
        let history = context.history().to_vec();
        assert!(context.append(0, vec![close("other", 1)]).is_err());
        assert_unchanged(&context, &before, &history);
        // Even a valid first close must not partially commit.
        assert!(
            context
                .append(0, vec![close("call-a", 1), close("call-a", 1)])
                .is_err()
        );
        assert_unchanged(&context, &before, &history);
        context.append(0, vec![close("call-a", 1)]).unwrap();
        let before = context.snapshot();
        let history = context.history().to_vec();
        assert!(context.append(1, vec![close("call-a", 1)]).is_err());
        assert_unchanged(&context, &before, &history);
        assert!(Context::new(limits(), vec![close("missing", 1)]).is_err());
    }

    #[test]
    fn duplicate_open_ids_are_not_reusable_in_the_active_view() {
        assert!(Context::new(limits(), vec![open("a", 1), open("a", 1)]).is_err());
        let mut context = Context::new(limits(), vec![open("a", 1), close("a", 1)]).unwrap();
        let before = context.snapshot();
        let history = context.history().to_vec();
        assert!(context.append(0, vec![open("a", 1)]).is_err());
        assert_unchanged(&context, &before, &history);
    }

    #[test]
    fn opaque_ids_and_atomic_pairs_are_supported() {
        let mut atomic = item("atomic pair", 1);
        atomic.opens = vec!["调用/🤔".into()];
        atomic.closes = atomic.opens.clone();
        let mut context = Context::new(limits(), vec![atomic]).unwrap();
        context.append(0, vec![open("", 1), close("", 1)]).unwrap();
        assert_eq!(context.snapshot().used, 3);
        let mut duplicate = item("bad close", 1);
        duplicate.closes = vec!["x".into(), "x".into()];
        assert!(Context::new(limits(), vec![open("x", 1), duplicate]).is_err());
    }

    #[test]
    fn balanced_replacement_strictly_reduces_units_and_retains_originals() {
        let original = vec![open("a", 10), close("a", 20)];
        let seed = vec![
            pinned("header", 10),
            original[0].clone(),
            original[1].clone(),
            item("tail", 10),
        ];
        let mut context = Context::new(limits(), seed.clone()).unwrap();
        let summary = vec![item("summary", 5)];
        let snapshot = context.replace(0, 1, 3, summary.clone()).unwrap();
        assert_eq!(snapshot.revision, 1);
        assert_eq!(snapshot.used, 25);
        assert_eq!(
            snapshot.items,
            vec![seed[0].clone(), summary[0].clone(), seed[3].clone()]
        );
        assert_eq!(
            context.history()[0],
            ContextEntry::Seed {
                revision: 0,
                items: seed
            }
        );
        assert_eq!(
            context.history()[1],
            ContextEntry::Replace {
                revision: 1,
                start: 1,
                end: 3,
                original,
                replacement: summary,
            }
        );
    }

    #[test]
    fn replacement_must_reduce_units_not_merely_item_count() {
        let mut context = Context::new(limits(), vec![item("a", 10), item("b", 10)]).unwrap();
        let before = context.snapshot();
        let history = context.history().to_vec();
        for units in [20, 21] {
            assert!(
                context
                    .replace(0, 0, 2, vec![item("not smaller", units)])
                    .is_err()
            );
            assert_unchanged(&context, &before, &history);
        }
        // More items are allowed when their combined units are strictly lower.
        let snapshot = context
            .replace(0, 0, 2, vec![item("x", 1), item("y", 1), item("z", 1)])
            .unwrap();
        assert_eq!(snapshot.used, 3);
        assert_eq!(snapshot.items.len(), 3);
    }

    #[test]
    fn invalid_replacement_ranges_do_not_panic_or_mutate() {
        let mut context = Context::new(limits(), vec![item("a", 10)]).unwrap();
        let before = context.snapshot();
        let history = context.history().to_vec();
        for (start, end) in [(0, 0), (1, 0), (0, 2), (usize::MAX, usize::MAX)] {
            assert!(context.replace(0, start, end, vec![]).is_err());
            assert_unchanged(&context, &before, &history);
        }
    }

    #[test]
    fn replacement_can_remove_a_whole_balanced_span() {
        let mut context = Context::new(limits(), vec![open("a", 10), close("a", 10)]).unwrap();
        let snapshot = context.replace(0, 0, 2, vec![]).unwrap();
        assert_eq!(snapshot.used, 0);
        assert!(snapshot.items.is_empty());
        assert_eq!(context.history().len(), 2);
    }

    #[test]
    fn tool_calls_and_results_cannot_be_split_at_either_boundary() {
        let mut context = Context::new(
            limits(),
            vec![open("a", 10), item("between", 10), close("a", 10)],
        )
        .unwrap();
        let before = context.snapshot();
        let history = context.history().to_vec();
        for (start, end) in [(0, 1), (0, 2), (1, 2), (1, 3), (2, 3)] {
            assert!(
                context
                    .replace(0, start, end, vec![item("summary", 1)])
                    .is_err()
            );
            assert_unchanged(&context, &before, &history);
        }
        context
            .replace(0, 0, 3, vec![item("whole pair", 1)])
            .unwrap();
    }

    #[test]
    fn replacement_may_neither_forge_nor_erase_pending_opens() {
        let mut context =
            Context::new(limits(), vec![item("old", 20), open("pending", 10)]).unwrap();
        let before = context.snapshot();
        let history = context.history().to_vec();
        assert!(context.replace(0, 0, 1, vec![open("forged", 1)]).is_err());
        assert_unchanged(&context, &before, &history);
        assert!(context.replace(0, 0, 1, vec![close("pending", 1)]).is_err());
        assert_unchanged(&context, &before, &history);
        assert!(
            context
                .replace(0, 1, 2, vec![item("erased call", 1)])
                .is_err()
        );
        assert_unchanged(&context, &before, &history);
        assert!(
            context
                .replace(0, 0, 2, vec![item("erased call", 1)])
                .is_err()
        );
        assert_unchanged(&context, &before, &history);
        // A balanced replacement before a pending suffix is fine.
        context.replace(0, 0, 1, vec![item("summary", 1)]).unwrap();
        context.append(1, vec![close("pending", 1)]).unwrap();
    }

    #[test]
    fn replacement_validates_pair_ids_against_the_entire_resulting_view() {
        let mut context = Context::new(
            limits(),
            vec![item("old", 20), open("a", 10), close("a", 10)],
        )
        .unwrap();
        let before = context.snapshot();
        let history = context.history().to_vec();
        assert!(
            context
                .replace(0, 0, 1, vec![open("a", 1), close("a", 1)])
                .is_err()
        );
        assert_unchanged(&context, &before, &history);
        // The selected closed pair may be replaced with a smaller full pair.
        context
            .replace(0, 1, 3, vec![open("a", 1), close("a", 1)])
            .unwrap();
    }

    #[test]
    fn pinned_items_cannot_be_changed_or_removed() {
        let mut context =
            Context::new(limits(), vec![pinned("fixed", 20), item("old", 10)]).unwrap();
        let before = context.snapshot();
        let history = context.history().to_vec();
        assert!(context.replace(0, 0, 1, vec![item("new", 1)]).is_err());
        assert_unchanged(&context, &before, &history);
        assert!(context.replace(0, 0, 2, vec![pinned("fixed", 20)]).is_err());
        assert_unchanged(&context, &before, &history);
        context.replace(0, 1, 2, vec![item("small", 1)]).unwrap();
        assert_eq!(context.snapshot().items[0], pinned("fixed", 20));
    }

    #[test]
    fn pinned_suffix_must_not_move_to_another_index() {
        let seed = vec![item("old-a", 10), item("old-b", 10), pinned("fixed", 10)];
        let mut context = Context::new(limits(), seed.clone()).unwrap();
        let before = context.snapshot();
        let history = context.history().to_vec();
        assert!(context.replace(0, 0, 2, vec![item("summary", 1)]).is_err());
        assert_unchanged(&context, &before, &history);
        context
            .replace(0, 0, 2, vec![item("a", 1), item("b", 1)])
            .unwrap();
        assert_eq!(context.snapshot().items[2], seed[2]);
    }

    #[test]
    fn release_view_keeps_history_and_invalidates_old_revisions() {
        let seed = vec![pinned("fixed", 10), open("pending", 10)];
        let mut context = Context::new(limits(), seed.clone()).unwrap();
        let snapshot = context.release_view().unwrap();
        assert_eq!(snapshot.revision, 1);
        assert_eq!(snapshot.used, 0);
        assert!(!snapshot.pressure);
        assert!(snapshot.items.is_empty());
        assert_eq!(
            context.history()[1],
            ContextEntry::ReleaseView {
                revision: 1,
                original: seed
            }
        );
        assert_eq!(
            context.append(0, vec![]).unwrap_err().code,
            "revision_conflict"
        );
        let replay = Context::replay(limits(), context.history()).unwrap();
        assert_eq!(replay.snapshot(), context.snapshot());
        assert_eq!(replay.history(), context.history());
    }

    #[test]
    fn recovered_context_can_continue_appending_and_replacing() {
        let mut context =
            Context::new(limits(), vec![pinned("header", 10), item("old", 20)]).unwrap();
        context.append(0, vec![open("pending", 10)]).unwrap();
        let mut recovered = Context::replay(limits(), context.history()).unwrap();
        let snapshot = recovered.append(1, vec![close("pending", 10)]).unwrap();
        assert_eq!(snapshot.revision, 2);
        let snapshot = recovered
            .replace(2, 1, 4, vec![item("summary", 1)])
            .unwrap();
        assert_eq!(snapshot.revision, 3);
        assert_eq!(snapshot.used, 11);
        assert_eq!(
            snapshot.items,
            vec![pinned("header", 10), item("summary", 1)]
        );
        // Replaying a recovered-and-continued log yields precisely the new state.
        let again = Context::replay(limits(), recovered.history()).unwrap();
        assert_eq!(again.snapshot(), recovered.snapshot());
        assert_eq!(again.history(), recovered.history());
        // Recovering does not mutate or alias the source context.
        assert_eq!(context.snapshot().revision, 1);
        assert_eq!(context.snapshot().items.len(), 3);
    }

    #[test]
    fn full_history_round_trips_through_json_and_deterministic_replay() {
        let mut context =
            Context::new(limits(), vec![pinned("header", 10), item("old", 20)]).unwrap();
        context
            .append(0, vec![open("a", 10), close("a", 10), item("tail", 10)])
            .unwrap();
        context.replace(1, 1, 4, vec![item("summary", 5)]).unwrap();
        context
            .replace(2, 1, 2, vec![item("shorter summary", 1)])
            .unwrap();
        let replay = Context::replay(limits(), context.history()).unwrap();
        assert_eq!(replay.snapshot(), context.snapshot());
        assert_eq!(replay.history(), context.history());
        // Releasing and reusing this standalone storage primitive also replays.
        context.release_view().unwrap();
        context.append(4, vec![item("new view", 1)]).unwrap();
        let encoded = serde_json::to_string(context.history()).unwrap();
        let entries: Vec<ContextEntry> = serde_json::from_str(&encoded).unwrap();
        let replay = Context::replay(limits(), entries).unwrap();
        assert_eq!(replay.snapshot(), context.snapshot());
        assert_eq!(replay.history(), context.history());
        assert!(encoded.contains("old"));
        assert!(encoded.contains("shorter summary"));
    }

    #[test]
    fn replay_rejects_missing_seeds_revision_gaps_and_extra_seeds() {
        assert!(Context::replay(limits(), Vec::<ContextEntry>::new()).is_err());
        assert!(
            Context::replay(
                limits(),
                vec![ContextEntry::Seed {
                    revision: 1,
                    items: vec![]
                }]
            )
            .is_err()
        );
        let seed = ContextEntry::Seed {
            revision: 0,
            items: vec![item("old", 20)],
        };
        assert!(
            Context::replay(
                limits(),
                vec![
                    seed.clone(),
                    ContextEntry::Append {
                        revision: 2,
                        items: vec![]
                    }
                ]
            )
            .is_err()
        );
        assert!(
            Context::replay(
                limits(),
                vec![
                    seed,
                    ContextEntry::Seed {
                        revision: 1,
                        items: vec![]
                    }
                ]
            )
            .is_err()
        );
    }

    #[test]
    fn replay_rejects_tampered_originals_and_invalid_operations() {
        let seed = ContextEntry::Seed {
            revision: 0,
            items: vec![item("old", 20)],
        };
        assert!(
            Context::replay(
                limits(),
                vec![
                    seed.clone(),
                    ContextEntry::Replace {
                        revision: 1,
                        start: 0,
                        end: 1,
                        original: vec![item("forged original", 20)],
                        replacement: vec![item("summary", 1)],
                    }
                ]
            )
            .is_err()
        );
        assert!(
            Context::replay(
                limits(),
                vec![
                    seed.clone(),
                    ContextEntry::ReleaseView {
                        revision: 1,
                        original: vec![],
                    }
                ]
            )
            .is_err()
        );
        assert!(
            Context::replay(
                limits(),
                vec![
                    seed.clone(),
                    ContextEntry::Replace {
                        revision: 1,
                        start: 0,
                        end: 1,
                        original: vec![item("old", 20)],
                        replacement: vec![item("not smaller", 21)],
                    }
                ]
            )
            .is_err()
        );
        assert!(
            Context::replay(
                limits(),
                vec![
                    seed,
                    ContextEntry::Append {
                        revision: 1,
                        items: vec![close("orphan", 1)],
                    }
                ]
            )
            .is_err()
        );
    }

    #[test]
    fn revision_exhaustion_fails_atomically_including_release() {
        let mut context = Context::new(limits(), vec![item("old", 20)]).unwrap();
        // Reaching this requires u64::MAX writes; inject it only in this test.
        context.revision = u64::MAX;
        let before = context.snapshot();
        let history = context.history().to_vec();
        assert_eq!(
            context.append(u64::MAX, vec![]).unwrap_err().code,
            "revision_exhausted"
        );
        assert_unchanged(&context, &before, &history);
        assert!(
            context
                .replace(u64::MAX, 0, 1, vec![item("short", 1)])
                .is_err()
        );
        assert_unchanged(&context, &before, &history);
        assert!(context.release_view().is_err());
        assert_unchanged(&context, &before, &history);
    }

    #[test]
    fn contexts_and_snapshot_copies_are_independent() {
        let seed = vec![item("common", 10)];
        let mut first = Context::new(limits(), seed.clone()).unwrap();
        let second = Context::new(limits(), seed.clone()).unwrap();
        let mut snapshot = first.snapshot();
        snapshot.items[0].data = json!("changed outside context");
        first.append(0, vec![item("only first", 10)]).unwrap();
        assert_eq!(second.snapshot().revision, 0);
        assert_eq!(second.snapshot().used, 10);
        assert_eq!(second.snapshot().items, seed);
        assert_eq!(first.snapshot().items[0].data, json!("common"));
        assert_eq!(second.history().len(), 1);
    }

    #[test]
    fn caller_content_is_never_normalized_or_reordered() {
        let original = ContextItem {
            data: json!({"text": "\r\n  空白 🤔 \t", "metadata": [1, true, null]}),
            units: 10,
            opens: vec![],
            closes: vec![],
            pinned: false,
        };
        let mut context = Context::new(limits(), vec![original.clone()]).unwrap();
        context.append(0, vec![original.clone()]).unwrap();
        assert_eq!(context.snapshot().items, vec![original.clone(), original]);
        let replay = Context::replay(limits(), context.history()).unwrap();
        assert_eq!(replay.snapshot(), context.snapshot());
    }

    #[test]
    fn zero_unit_items_and_empty_appends_are_logged() {
        let mut context = Context::new(limits(), vec![item("zero", 0)]).unwrap();
        let snapshot = context.append(0, vec![]).unwrap();
        assert_eq!(snapshot.revision, 1);
        assert_eq!(snapshot.used, 0);
        assert_eq!(context.history().len(), 2);
        let before = context.snapshot();
        let history = context.history().to_vec();
        assert!(context.replace(1, 0, 1, vec![]).is_err());
        assert_unchanged(&context, &before, &history);
    }
}
