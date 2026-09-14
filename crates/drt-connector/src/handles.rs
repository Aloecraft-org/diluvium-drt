//! Host-held resources with an owner: the table every connector that keeps
//! state on a guest's behalf keys its state in.
//!
//! # Surface
//!
//! Entry points:
//! - [`Handles::new`] — one table per resource kind, labelled by name.
//! - [`Handles::insert`] — give `caller` a handle to `resource`.
//! - [`Handles::with`] — use one of `caller`'s resources, by handle.
//! - [`Handles::remove`] — take one back, by handle.
//! - [`Handles::rekey`] — move one to another owner, number unchanged.
//! - [`Handles::take_where`] — sweep out what a predicate says is done.
//! - [`Handles::release`] — take back everything one node owns. The death
//!   path (`doc/Plan-0.7.0.md` §2.4).
//! - [`Handles::drain_root`] — take back everything the root owns. The
//!   teardown path, for `Connector::finish`.
//!
//! Configurable values: none. Handle ids are never reused within a table.
//!
//! Fan-out: [`Caller`] is the only branch — `Root` or `Node`, and nothing
//! else, because a host-held resource can follow the root's lifetime or a
//! node's and there is no third (§2.3).
//!
//! # The two rules this enforces, and why they are here and not in each
//! connector
//!
//! **A handle belongs to exactly one caller** (§2.2). Lookup is by
//! `(caller, handle)`, so a node presenting a number it did not receive
//! gets [`NoSuchHandle`] — not a permission failure, because from where it
//! stands the handle does not exist. A denial would tell it something true
//! about another node. The error message is a constant for that reason:
//! it must not vary with whether the handle exists elsewhere.
//!
//! **Release is by owner, and root-owned survives it** (§2.3–2.4).
//! [`Handles::release`] takes a node and drains only that node's entries.
//! [`Caller::Root`] matches no node, so root-scoped resources live until
//! [`Handles::drain_root`] at teardown, without a second code path.
//!
//! **This file names no resource.** `R` is whatever a connector holds — a
//! socket, a derived key, a plugin process — and the table does not care.
//! That is deliberate (§7.0): the ownership dimension is built once,
//! resource-agnostic, and each consumer is a type parameter rather than a
//! reimplementation.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// Who is asking, and therefore who owns what the call creates.
///
/// `Node` carries the swarm's instance id as a bare `u32` rather than the
/// swarm's own type, because this crate sits below `drt-swarm` and a
/// dependency the other way would be a cycle. It is the same number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Caller {
    /// No instance is calling: a harness, a test, the process's own
    /// bookkeeping. What it creates is the root's and lives to teardown.
    Root,
    /// An instance, by the swarm's id. What it creates dies with it.
    Node(u32),
}

impl std::fmt::Display for Caller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Caller::Root => f.write_str("the root"),
            Caller::Node(id) => write!(f, "instance {id}"),
        }
    }
}

/// An opaque handle. The guest sees the number; the number means nothing
/// without the caller it was issued to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HandleId(pub u64);

/// The one thing a wrong handle is told. A constant, so it cannot leak
/// whether the number is live under another owner.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("no such handle")]
pub struct NoSuchHandle;

/// Every `R` this connector holds, and whose each one is.
///
/// Interior mutability because a connector's `call` takes `&self`; the
/// lock is per table and held only for the duration of one operation.
pub struct Handles<R> {
    kind: &'static str,
    inner: Mutex<Inner<R>>,
}

struct Inner<R> {
    next: u64,
    table: BTreeMap<(Caller, HandleId), R>,
}

impl<R> Handles<R> {
    /// A table for one kind of resource. `kind` is a word for messages —
    /// `"socket"`, `"key"` — and appears in what [`Handles::release`]
    /// reports lost.
    pub fn new(kind: &'static str) -> Self {
        Handles {
            kind,
            inner: Mutex::new(Inner {
                next: 1,
                table: BTreeMap::new(),
            }),
        }
    }

    /// The kind this table was made for.
    pub fn kind(&self) -> &'static str {
        self.kind
    }

    /// Give `caller` a handle to `resource`. Ids start at 1 and are never
    /// reused within a table, so a stale number can never alias a live one.
    pub fn insert(&self, caller: Caller, resource: R) -> HandleId {
        let mut inner = self.lock();
        let id = HandleId(inner.next);
        inner.next += 1;
        inner.table.insert((caller, id), resource);
        id
    }

    /// Use one of `caller`'s resources. A handle `caller` was not issued —
    /// whether it belongs to someone else or to no one — is
    /// [`NoSuchHandle`], and the two cases are indistinguishable by design.
    pub fn with<T>(
        &self,
        caller: Caller,
        handle: HandleId,
        f: impl FnOnce(&mut R) -> T,
    ) -> Result<T, NoSuchHandle> {
        let mut inner = self.lock();
        inner
            .table
            .get_mut(&(caller, handle))
            .map(f)
            .ok_or(NoSuchHandle)
    }

    /// Take one of `caller`'s resources back.
    pub fn remove(&self, caller: Caller, handle: HandleId) -> Result<R, NoSuchHandle> {
        self.lock()
            .table
            .remove(&(caller, handle))
            .ok_or(NoSuchHandle)
    }

    /// Move one of `from`'s resources to `to`, **number unchanged**. Ids
    /// are unique within a table, so the same number under a new owner
    /// aliases nothing; from this call on, `from` presenting it gets
    /// [`NoSuchHandle`] like anyone else. The transfer primitive
    /// (`doc/Plan-0.7.0.md` §3.2), resource-agnostic like the rest.
    pub fn rekey(&self, from: Caller, to: Caller, handle: HandleId) -> Result<(), NoSuchHandle> {
        let mut inner = self.lock();
        let resource = inner.table.remove(&(from, handle)).ok_or(NoSuchHandle)?;
        inner.table.insert((to, handle), resource);
        Ok(())
    }

    /// Everything `caller` owns, taken back, with each one's handle. The
    /// death path: the connector decides what, if anything, was lost by
    /// each resource going away, and says so in its own words.
    ///
    /// Root-owned entries are not touched here — see [`Handles::drain_root`].
    pub fn release(&self, caller: Caller) -> Vec<(HandleId, R)> {
        let mut inner = self.lock();
        let keys: Vec<_> = inner
            .table
            .keys()
            .filter(|(owner, _)| *owner == caller)
            .copied()
            .collect();
        keys.into_iter()
            .filter_map(|key| inner.table.remove(&key).map(|r| (key.1, r)))
            .collect()
    }

    /// Everything the root owns, taken back. The teardown path.
    pub fn drain_root(&self) -> Vec<(HandleId, R)> {
        self.release(Caller::Root)
    }

    /// Take back every resource `keep` says is done with, whoever holds
    /// it, each with its owner and handle. The sweep a connector runs when
    /// it must notice something about what it holds without being asked —
    /// a vital resource that ended (`doc/Plan-0.7.0.md` §3.3). The
    /// predicate sees the resource mutably and is called under the table's
    /// lock, so it must be a look, not a wait.
    pub fn take_where(
        &self,
        mut done: impl FnMut(Caller, HandleId, &mut R) -> bool,
    ) -> Vec<(Caller, HandleId, R)> {
        let mut inner = self.lock();
        let keys: Vec<_> = inner
            .table
            .iter_mut()
            .filter_map(|((owner, handle), r)| {
                done(*owner, *handle, r).then_some((*owner, *handle))
            })
            .collect();
        keys.into_iter()
            .filter_map(|key| inner.table.remove(&key).map(|r| (key.0, key.1, r)))
            .collect()
    }

    /// How many resources `caller` holds. For tests and reports.
    pub fn count(&self, caller: Caller) -> usize {
        self.lock()
            .table
            .keys()
            .filter(|(owner, _)| *owner == caller)
            .count()
    }

    // depth: the lock, and why poisoning is treated as recoverable.

    /// A poisoned lock means a panic mid-operation. The table's invariants
    /// are simple enough that the map is still consistent after any panic
    /// point here, so recover rather than propagate: a connector that
    /// cannot look up its own table cannot report what it lost either.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner<R>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Acceptance 2: a node presenting a number it did not receive gets
    /// "no such handle", and the message says nothing about where the
    /// handle does live.
    #[test]
    fn a_handle_is_invisible_to_a_node_that_does_not_own_it() {
        let table: Handles<String> = Handles::new("thing");
        let a = Caller::Node(1);
        let b = Caller::Node(2);
        let h = table.insert(a, "a's".into());

        assert_eq!(table.with(a, h, |s| s.clone()).unwrap(), "a's");

        let err = table.with(b, h, |s| s.clone()).unwrap_err();
        assert_eq!(err, NoSuchHandle);
        let said = err.to_string();
        assert_eq!(said, "no such handle");
        assert!(
            !said.contains('1') && !said.contains("instance") && !said.contains("other"),
            "the refusal must not name the real owner: {said}"
        );

        // And the same sentence for a number nobody was ever issued.
        let never = table.with(b, HandleId(999), |s| s.clone()).unwrap_err();
        assert_eq!(
            never.to_string(),
            said,
            "existing-elsewhere and nonexistent read alike"
        );
    }

    /// §2.3–2.4: release is by owner, and root-owned survives it.
    #[test]
    fn release_takes_one_nodes_resources_and_leaves_the_roots() {
        let table: Handles<u8> = Handles::new("thing");
        let node = Caller::Node(7);
        let other = Caller::Node(8);
        table.insert(node, 1);
        table.insert(node, 2);
        table.insert(other, 3);
        table.insert(Caller::Root, 4);

        let mut released: Vec<u8> = table.release(node).into_iter().map(|(_, r)| r).collect();
        released.sort_unstable();
        assert_eq!(released, vec![1, 2], "exactly the node's, no more");

        assert_eq!(table.count(node), 0);
        assert_eq!(table.count(other), 1, "a sibling is untouched");
        assert_eq!(
            table.count(Caller::Root),
            1,
            "the root's survives a node's death"
        );

        let root: Vec<u8> = table.drain_root().into_iter().map(|(_, r)| r).collect();
        assert_eq!(root, vec![4], "and is taken back at teardown");
    }

    /// A released handle cannot be used afterwards, and its number is not
    /// handed out again.
    #[test]
    fn a_released_handle_is_gone_and_its_number_is_not_reused() {
        let table: Handles<u8> = Handles::new("thing");
        let node = Caller::Node(1);
        let h1 = table.insert(node, 1);
        table.release(node);
        assert_eq!(table.with(node, h1, |r| *r), Err(NoSuchHandle));
        let h2 = table.insert(node, 2);
        assert_ne!(h1, h2, "ids are never reused within a table");
    }

    /// §3.2: a rekey moves the resource, keeps the number, and leaves the
    /// old owner with the same sentence as a stranger.
    #[test]
    fn rekey_moves_a_resource_and_keeps_its_number() {
        let table: Handles<u8> = Handles::new("thing");
        let a = Caller::Node(1);
        let b = Caller::Node(2);
        let h = table.insert(a, 5);
        assert_eq!(table.rekey(a, b, h), Ok(()));
        assert_eq!(table.with(b, h, |r| *r), Ok(5), "same number, new owner");
        assert_eq!(table.with(a, h, |r| *r), Err(NoSuchHandle));
        assert_eq!(
            table.rekey(a, b, h),
            Err(NoSuchHandle),
            "and cannot be moved twice"
        );
        assert_eq!(table.count(a), 0);
        assert_eq!(table.count(b), 1);
    }

    /// §3.3: a sweep takes exactly what the predicate names, across owners,
    /// and says whose each one was.
    #[test]
    fn take_where_sweeps_across_owners_and_names_each() {
        let table: Handles<u8> = Handles::new("thing");
        let a = Caller::Node(1);
        let b = Caller::Node(2);
        let ha = table.insert(a, 10);
        table.insert(a, 3);
        let hb = table.insert(b, 12);
        let mut taken = table.take_where(|_, _, r| *r >= 10);
        taken.sort_unstable_by_key(|(_, h, _)| *h);
        assert_eq!(taken, vec![(a, ha, 10), (b, hb, 12)]);
        assert_eq!(table.count(a), 1);
        assert_eq!(table.count(b), 0);
        assert!(
            table.take_where(|_, _, r| *r >= 10).is_empty(),
            "swept once"
        );
    }

    #[test]
    fn remove_takes_back_exactly_one() {
        let table: Handles<u8> = Handles::new("thing");
        let node = Caller::Node(1);
        let h = table.insert(node, 9);
        assert_eq!(table.remove(node, h), Ok(9));
        assert_eq!(table.remove(node, h), Err(NoSuchHandle));
        assert_eq!(table.remove(Caller::Node(2), h), Err(NoSuchHandle));
    }
}
