//! Groups tracked reviews into stacks, derived from each review's `ancestors` on every call
//! rather than stored: membership changes as revisions land or get reordered, so a stored copy
//! would just go stale.
//!
//! A stack is a root-to-leaf chain. A lone review is a stack of one. If a chain forks (two
//! reviews share a parent), each leaf gets its own stack, and the shared ancestors belong to
//! whichever leaf sorts first.

use std::collections::{BTreeMap, BTreeSet};

use crate::state::{ReviewEntry, ReviewKey};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stack {
    /// The top-most member - the review a workspace for this stack is built from.
    pub tip: ReviewKey,
    /// The tracked members, bottom-most first. Ancestors that aren't tracked (you're not a
    /// reviewer on them) don't appear here, though they're part of the stack's contents.
    pub members: Vec<ReviewKey>,
}

/// Group `entries` into stacks. Resolved entries are ignored: a landed review is already part of
/// its descendants' base, not of the stack.
pub fn group<'a>(entries: impl IntoIterator<Item = &'a ReviewEntry>) -> Vec<Stack> {
    let live: BTreeMap<&ReviewKey, &ReviewEntry> = entries
        .into_iter()
        .filter(|e| !e.resolved)
        .map(|e| (&e.key, e))
        .collect();

    let ancestors_of_something: BTreeSet<&ReviewKey> = live
        .values()
        .flat_map(|e| e.ancestors.iter())
        .collect();

    let mut assigned: BTreeSet<&ReviewKey> = BTreeSet::new();
    let mut stacks = Vec::new();
    // `live` is a BTreeMap, so leaves are visited in key order: deterministic.
    for (key, entry) in &live {
        if ancestors_of_something.contains(key) {
            continue;
        }
        let members: Vec<ReviewKey> = entry
            .ancestors
            .iter()
            .chain(std::iter::once(*key))
            .filter(|k| live.contains_key(k) && assigned.insert(k))
            .cloned()
            .collect();
        stacks.push(Stack {
            tip: (*key).clone(),
            members,
        });
    }

    // Inconsistent data (an ancestor list that doesn't reach a member) shouldn't hide a review.
    for key in live.keys() {
        if assigned.insert(key) {
            stacks.push(Stack {
                tip: (*key).clone(),
                members: vec![(*key).clone()],
            });
        }
    }
    stacks
}

/// The stack containing `key`, if it's tracked and unresolved.
pub fn stack_containing<'a>(stacks: &'a [Stack], key: &ReviewKey) -> Option<&'a Stack> {
    stacks.iter().find(|s| s.members.contains(key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{RepoRef, ReviewKind};

    fn k(id: &str) -> ReviewKey {
        ReviewKey::new("moz", id)
    }

    fn entry(id: &str, ancestors: &[&str]) -> ReviewEntry {
        ReviewEntry {
            key: k(id),
            title: String::new(),
            author: String::new(),
            url: String::new(),
            repo: RepoRef {
                urls: vec![],
                display_name: String::new(),
            },
            kind: ReviewKind::Direct,
            version: "1".into(),
            in_queue: true,
            resolved: false,
            last_synced: chrono::Utc::now(),
            stack_id: None,
            ancestors: ancestors.iter().map(|a| k(a)).collect(),
            diff_stat: None,
            description: None,
        }
    }

    #[test]
    fn lone_reviews_are_stacks_of_one() {
        let entries = [entry("D1", &[]), entry("D2", &[])];
        let stacks = group(&entries);
        assert_eq!(stacks.len(), 2);
        assert_eq!(stacks[0].members, vec![k("D1")]);
    }

    #[test]
    fn chain_groups_bottom_to_top() {
        let entries = [
            entry("D3", &["D1", "D2"]),
            entry("D1", &[]),
            entry("D2", &["D1"]),
        ];
        let stacks = group(&entries);
        assert_eq!(stacks.len(), 1);
        assert_eq!(stacks[0].tip, k("D3"));
        assert_eq!(stacks[0].members, vec![k("D1"), k("D2"), k("D3")]);
    }

    #[test]
    fn untracked_ancestors_are_skipped_but_still_link_members() {
        // D9 isn't tracked (not our review), yet D1 and D3 are still one stack through it.
        let entries = [entry("D1", &[]), entry("D3", &["D1", "D9"])];
        let stacks = group(&entries);
        assert_eq!(stacks.len(), 1);
        assert_eq!(stacks[0].members, vec![k("D1"), k("D3")]);
    }

    #[test]
    fn a_fork_gives_each_leaf_its_own_stack_and_the_shared_parent_to_the_first() {
        let entries = [
            entry("D1", &[]),
            entry("D2", &["D1"]),
            entry("D3", &["D1"]),
        ];
        let stacks = group(&entries);
        assert_eq!(stacks.len(), 2);
        assert_eq!(stacks[0].members, vec![k("D1"), k("D2")]);
        assert_eq!(stacks[1].members, vec![k("D3")]);
    }

    #[test]
    fn resolved_entries_are_ignored() {
        let mut d1 = entry("D1", &[]);
        d1.resolved = true;
        let entries = [d1, entry("D2", &[])];
        let stacks = group(&entries);
        assert_eq!(stacks.len(), 1);
        assert_eq!(stacks[0].members, vec![k("D2")]);
    }
}
