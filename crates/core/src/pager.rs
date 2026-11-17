//! Paging rules for long lists (library grid, a person's works, the continue-watching wall), as a state machine
//! with no I/O: [`Pager::next_page`] says what to fetch ([`crate::browse::page`] fetches it), [`Pager::apply`]
//! merges the reply.
//!
//! Some servers answer resumable queries with TotalRecordCount = the number returned, and some ignore StartIndex;
//! these rules cope with both. Once StartIndex is found ignored, each page asks from 0 for one page more and keeps
//! the tail (transfer grows with the list, fine for the lists this hits; a server that also caps Limit cannot be
//! read past its cap by any request, so the list ends there).

use crate::browse::{Item, Page};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

/// Items per page.
pub const PAGE_SIZE: usize = 60;

/// A non-empty page made only of items already shown: the server ignored StartIndex.
pub fn ignored_start(shown: &[Item], p: &Page) -> bool {
    let ids: HashSet<&str> = shown.iter().map(|i| i.id.as_str()).collect();
    !p.items.is_empty() && p.items.iter().all(|i| ids.contains(i.id.as_str()))
}

/// What [`merge`] makes of a page.
#[derive(Debug, Clone, PartialEq)]
pub struct Merged {
    /// Items of the page not shown yet, in order.
    pub fresh: Vec<Item>,
    /// The list has ended.
    pub done: bool,
    /// What to show as the size of the list; exact once done.
    pub total: usize,
    /// `total` is the real size of the list, not just what has loaded.
    pub exact: bool,
}

/// Merges a page into what is shown. `skip`: leading items of the reply that are not part of this page (a server
/// ignoring StartIndex is asked from 0); the total is judged against the whole reply. Items already shown are
/// dropped (a list sorted by last played can shift between pages). The list ends on a short page, or once a
/// total that is plainly real (more than the reply holds) is reached.
pub fn merge(shown: &[Item], p: Page, size: usize, skip: usize) -> Merged {
    let ids: HashSet<&str> = shown.iter().map(|i| i.id.as_str()).collect();
    let returned = p.items.len();
    let got: Vec<Item> = p.items.into_iter().skip(skip).collect();
    let got_n = got.len();
    let mut seen = HashSet::new();
    let fresh: Vec<Item> = got
        .into_iter()
        .filter(|i| !ids.contains(i.id.as_str()) && seen.insert(i.id.clone()))
        .collect();
    let n = shown.len() + fresh.len();
    // An honest total of exactly one full page looks like a reported count: costs one empty request, nothing worse
    let real = p.total_record_count > returned;
    let done = got_n < size || (real && n >= p.total_record_count);
    let total = if done {
        n
    } else {
        n.max(if real { p.total_record_count } else { 0 })
    };
    Merged {
        fresh,
        done,
        total,
        exact: done || real,
    }
}

/// A list's first page as stored in a snapshot ([`crate::snapshot`]), so a grid draws at once on the next visit.
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct Snap {
    pub items: Vec<Item>,
    pub total: usize,
    pub exact: bool,
    pub done: bool,
}

/// One request a [`Pager`] wants: `StartIndex = index`, `Limit = limit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fetch {
    /// The pager that asked: a reply for a pager since replaced (new sort, refresh) is ignored.
    pager: u64,
    /// Where this page starts in the list (differs from `index` once StartIndex is found ignored).
    start: usize,
    pub index: usize,
    pub limit: usize,
}

/// What [`Pager::apply`] did with a reply.
#[derive(Debug, Clone, PartialEq)]
pub enum Applied {
    /// Merged. `Some` when it was the first page: store it as the list's snapshot.
    Merged(Option<Snap>),
    /// The server ignored StartIndex: send this request instead (asks from 0, keeps the tail).
    Again(Fetch),
    /// A reply for another pager or request: nothing changed.
    Stale,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The state of one paged list for one query. A new query (sort, filter) or a manual refresh makes a new one, so
/// replies to the old one are dropped. After a failure [`Pager::next_page`] gives nothing until [`Pager::retry`], so a
/// grid cannot retry in a loop.
#[derive(Debug)]
pub struct Pager {
    id: u64,
    size: usize,
    items: Vec<Item>,
    total: usize,
    exact: bool,
    done: bool,
    /// Something to show: a snapshot or a fetched page.
    loaded: bool,
    /// Showing the first-page snapshot: the next fetch starts at 0 and replaces it.
    from_snap: bool,
    /// The server ignores StartIndex for this query.
    grow: bool,
    failed: bool,
    /// The request [`Pager::next_page`] handed out last, until its reply is applied or fails.
    pending: Option<Fetch>,
}

impl Pager {
    /// Starts a list, drawing from its snapshot if there is one.
    pub fn new(snap: Option<Snap>) -> Self {
        Self::with_size(snap, PAGE_SIZE)
    }

    fn with_size(snap: Option<Snap>, size: usize) -> Self {
        let from_snap = snap.is_some();
        let s = snap.unwrap_or_default();
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            size,
            items: s.items,
            total: s.total,
            exact: s.exact,
            done: s.done,
            loaded: from_snap,
            from_snap,
            grow: false,
            failed: false,
            pending: None,
        }
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// The size to show; with [`Pager::exact`] false it is a lower bound (shown as `N+`).
    pub fn total(&self) -> usize {
        self.total
    }

    pub fn exact(&self) -> bool {
        self.exact
    }

    /// Nothing more to fetch (a snapshot's `done` still refetches its first page).
    pub fn done(&self) -> bool {
        self.done && !self.from_snap
    }

    /// There is something to show (a snapshot or a fetched page); before that the grid shows a skeleton.
    pub fn loaded(&self) -> bool {
        self.loaded
    }

    /// A request is out.
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }

    /// The last request failed and [`Pager::retry`] has not been called.
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// The next request: `None` while one is out, after a failure, or once the list has ended.
    pub fn next_page(&mut self) -> Option<Fetch> {
        if self.failed || self.pending.is_some() || (self.loaded && self.done()) {
            return None;
        }
        let start = if self.from_snap { 0 } else { self.items.len() };
        let f = self.fetch(start);
        self.pending = Some(f);
        Some(f)
    }

    fn fetch(&self, start: usize) -> Fetch {
        let (index, limit) = if self.grow {
            (0, start + self.size)
        } else {
            (start, self.size)
        };
        Fetch {
            pager: self.id,
            start,
            index,
            limit,
        }
    }

    /// Takes the reply to `f`.
    pub fn apply(&mut self, f: Fetch, p: Page) -> Applied {
        if self.pending != Some(f) {
            return Applied::Stale;
        }
        let shown: &[Item] = if f.start > 0 { &self.items } else { &[] };
        if f.start > 0 && !self.grow && ignored_start(shown, &p) {
            self.grow = true;
            let again = self.fetch(f.start);
            self.pending = Some(again);
            return Applied::Again(again);
        }
        let m = merge(shown, p, self.size, if self.grow { f.start } else { 0 });
        if f.start == 0 {
            self.items = m.fresh;
            self.from_snap = false;
        } else {
            self.items.extend(m.fresh);
        }
        (self.done, self.total, self.exact) = (m.done, m.total, m.exact);
        self.loaded = true;
        self.pending = None;
        Applied::Merged((f.start == 0).then(|| Snap {
            items: self.items.clone(),
            total: self.total,
            exact: self.exact,
            done: self.done,
        }))
    }

    /// The request `f` failed. Returns whether to show the error: not while a snapshot is on screen (a refresh
    /// retries), as with the other pages.
    pub fn fail(&mut self, f: Fetch) -> bool {
        if self.pending != Some(f) {
            return false;
        }
        self.pending = None;
        self.failed = true;
        !self.from_snap
    }

    /// Allows [`Pager::next_page`] again after a failure (the grid's retry button).
    pub fn retry(&mut self) {
        self.failed = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(from: usize, n: usize) -> Vec<Item> {
        (from..from + n)
            .map(|k| Item {
                id: k.to_string(),
                name: "x".into(),
                kind: "Movie".into(),
                ..Default::default()
            })
            .collect()
    }

    fn page(items: Vec<Item>, total: usize) -> Page {
        Page {
            items,
            total_record_count: total,
        }
    }

    fn ids(v: &[Item]) -> Vec<&str> {
        v.iter().map(|i| i.id.as_str()).collect()
    }

    /// Honest total: shown while paging; a full last page ends the list without an extra request.
    #[test]
    fn honest_total() {
        let first = merge(&[], page(items(0, 3), 6), 3, 0);
        assert_eq!(
            (first.done, first.total, first.exact),
            (false, 6, true),
            "first page"
        );
        let last = merge(&items(0, 3), page(items(3, 3), 6), 3, 0);
        assert_eq!(ids(&last.fresh), ["3", "4", "5"], "second page is new");
        assert_eq!((last.done, last.total), (true, 6), "ends on the real total");
    }

    /// Total = number returned: a full page is not the end, the count grows, a short page ends it exactly.
    #[test]
    fn total_is_the_number_returned() {
        let first = merge(&[], page(items(0, 3), 3), 3, 0);
        assert_eq!(
            (first.done, first.total, first.exact),
            (false, 3, false),
            "first page"
        );
        let second = merge(&items(0, 3), page(items(3, 3), 3), 3, 0);
        assert_eq!((second.done, second.total), (false, 6), "second page");
        let last = merge(&items(0, 6), page(items(6, 2), 2), 3, 0);
        assert_eq!(
            (last.done, last.total, last.exact),
            (true, 8, true),
            "short page"
        );
        assert!(
            merge(&items(0, 6), page(vec![], 0), 3, 0).done,
            "an empty page ends it"
        );
    }

    /// A page of items already shown is an ignored StartIndex; a partly new page or an empty one is not.
    #[test]
    fn detects_ignored_start() {
        assert!(
            ignored_start(&items(0, 3), &page(items(0, 3), 3)),
            "same items"
        );
        assert!(
            !ignored_start(&items(0, 3), &page(items(2, 3), 3)),
            "partly new"
        );
        assert!(!ignored_start(&items(0, 3), &page(vec![], 0)), "empty");
    }

    /// Asked from 0 (StartIndex ignored): only the tail is the page, and the total is judged on the whole reply.
    #[test]
    fn asked_from_zero_keeps_the_tail() {
        let fake = merge(&items(0, 10), page(items(0, 20), 20), 10, 10);
        assert_eq!(ids(&fake.fresh), ids(&items(10, 10)), "the tail");
        assert_eq!((fake.done, fake.total), (false, 20), "reported count");
        let end = merge(&items(0, 20), page(items(0, 25), 25), 10, 20);
        assert_eq!(
            (end.fresh.len(), end.done, end.total),
            (5, true, 25),
            "short tail ends it"
        );
        let honest = merge(&items(0, 10), page(items(0, 20), 50), 10, 10);
        assert_eq!((honest.done, honest.total), (false, 50), "real total");
    }

    /// Items already shown, or repeated within a page, are dropped and not counted.
    #[test]
    fn drops_duplicates() {
        let mut p = items(2, 3);
        p.extend(items(4, 1));
        let r = merge(&items(0, 3), page(p, 10), 4, 0);
        assert_eq!(ids(&r.fresh), ["3", "4"], "only new ones, once");
        assert_eq!((r.done, r.total), (false, 10), "not the end");
    }

    /// Requests and merges through a list whose server ignores StartIndex and reports the returned count:
    /// `0/3 3/3 0/6 0/9`, every item once, a snapshot only of the first page.
    #[test]
    fn pager_switches_to_asking_from_zero() {
        let all = items(0, 8);
        let reply = |f: Fetch| {
            let v: Vec<Item> = all.iter().take(f.limit).cloned().collect(); // ignores StartIndex
            let n = v.len();
            page(v, n)
        };
        let mut pg = Pager::with_size(None, 3);
        let mut asked = Vec::new();
        let mut snaps = 0;
        while let Some(f) = pg.next_page() {
            assert!(pg.next_page().is_none(), "one request at a time");
            let mut f = f;
            loop {
                asked.push((f.index, f.limit));
                match pg.apply(f, reply(f)) {
                    Applied::Again(again) => f = again,
                    Applied::Merged(snap) => {
                        snaps += usize::from(snap.is_some());
                        break;
                    }
                    Applied::Stale => panic!("own reply is not stale"),
                }
            }
        }
        assert_eq!(asked, [(0, 3), (3, 3), (0, 6), (0, 9)], "requests");
        assert_eq!(ids(pg.items()), ids(&all), "every item once, in order");
        assert!(pg.done() && pg.exact() && pg.total() == 8, "ends exactly");
        assert_eq!(snaps, 1, "only the first page is a snapshot");
    }

    /// A snapshot draws at once, the first fetch replaces it from 0; a failure while it shows is not an error,
    /// and nothing is fetched again until retry.
    #[test]
    fn pager_snapshot_failure_and_retry() {
        let snap = Snap {
            items: items(0, 2),
            total: 2,
            exact: true,
            done: true,
        };
        let mut pg = Pager::with_size(Some(snap), 3);
        assert!(
            pg.loaded() && !pg.done(),
            "a snapshot shows but is refetched"
        );
        let f = pg.next_page().expect("refetch the first page");
        assert_eq!((f.index, f.limit), (0, 3), "from 0");
        assert!(!pg.fail(f), "no error over a snapshot");
        assert!(pg.failed() && pg.next_page().is_none(), "no retry loop");
        pg.retry();
        let f = pg.next_page().expect("after retry");
        assert!(
            matches!(pg.apply(f, page(items(5, 1), 1)), Applied::Merged(Some(_))),
            "first page"
        );
        assert_eq!(ids(pg.items()), ["5"], "snapshot replaced");
        assert!(pg.done() && pg.next_page().is_none(), "short page ends it");

        let mut fresh = Pager::with_size(None, 3);
        let f = fresh.next_page().expect("first page");
        assert!(fresh.fail(f), "an error without a snapshot");
    }

    /// Replies for a replaced pager, or for a request already answered, change nothing.
    #[test]
    fn pager_ignores_stale_replies() {
        let mut old = Pager::with_size(None, 3);
        let f = old.next_page().expect("first page");
        let mut new = Pager::with_size(None, 3);
        assert_eq!(
            new.apply(f, page(items(0, 3), 3)),
            Applied::Stale,
            "another pager"
        );
        assert!(!new.fail(f), "another pager's failure");
        assert!(
            matches!(old.apply(f, page(items(0, 3), 3)), Applied::Merged(_)),
            "own reply"
        );
        assert_eq!(
            old.apply(f, page(items(0, 3), 3)),
            Applied::Stale,
            "answered twice"
        );
        assert_eq!(old.items().len(), 3, "not merged twice");
    }
}
