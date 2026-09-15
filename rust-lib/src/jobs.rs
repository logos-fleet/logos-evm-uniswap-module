//! The board a deferred answer is parked on while its outbound call is in
//! flight — the state an async call site needs and a synchronous one did not.
//!
//! WHY THIS EXISTS. Every price/quote/swap method is one `eth_call` issued
//! through `eth_rpc_module`, and that call is now made with the generated
//! `_async` client (the only spelling a `web`/wasm image can have: one Worker,
//! one event loop, no ASYNCIFY — ADR 0004, so a call that blocked for its reply
//! would deadlock the loop that delivers it). An async call has no return
//! value, so a method that starts one cannot answer in the same breath. It
//! answers a JOB ID, and the caller collects with `take_result`.
//!
//! IT IS A `static`, NOT A FIELD, at the call sites: the callback the door
//! takes is `FnOnce + Send + 'static` and cannot borrow the module. That is
//! true of every async callback in the SDK, not a property of this module.
//!
//! No Logos/Qt dependency here on purpose — this compiles and is unit-tested
//! with `cargo test --no-default-features`, like `config`, `pricing` and `swap`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// How many jobs the board remembers at once.
///
/// A caller that starts jobs and never collects them must not be able to grow
/// this without bound — a module that leaks one slot per price refresh is a
/// wallet that leaks for as long as it is open. At the cap the OLDEST slot is
/// dropped, and a `take_result` for it then reads `Unknown` rather than hanging
/// on `Pending` for ever, which is the difference a caller can act on.
pub const CAPACITY: usize = 64;

/// What `take_result` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    /// The outbound call is still in flight. Ask again.
    Pending,
    /// The answer, handed over exactly once.
    Ready(String),
    /// Never issued, already collected, or evicted at [`CAPACITY`].
    Unknown,
}

/// Pending and completed answers, keyed by job id.
#[derive(Debug)]
pub struct JobBoard {
    next: AtomicU64,
    slots: Mutex<Slots>,
}

#[derive(Debug, Default)]
struct Slots {
    /// `None` = in flight, `Some(answer)` = completed and uncollected.
    ///
    /// A `BTreeMap` rather than a `HashMap` only because `BTreeMap::new` is a
    /// `const fn`, which is what lets the whole board be a plain `static` at
    /// the call sites instead of a lazily-initialised one.
    by_id: BTreeMap<String, Option<String>>,
    /// Insertion order, so eviction can name the oldest slot.
    order: VecDeque<String>,
}

impl Default for JobBoard {
    fn default() -> Self {
        Self::new()
    }
}

impl JobBoard {
    pub const fn new() -> Self {
        Self { next: AtomicU64::new(1), slots: Mutex::new(Slots::new()) }
    }

    /// Reserve an id and mark it in flight. The id is unique for the life of
    /// the process, so a stale `take_result` can never collect a later job's
    /// answer by reusing a number.
    pub fn start(&self) -> String {
        let id = format!("j{}", self.next.fetch_add(1, Ordering::Relaxed));
        let mut slots = self.slots.lock().unwrap();
        while slots.order.len() >= CAPACITY {
            let Some(oldest) = slots.order.pop_front() else { break };
            slots.by_id.remove(&oldest);
        }
        slots.by_id.insert(id.clone(), None);
        slots.order.push_back(id.clone());
        id
    }

    /// Park the answer for `id`. A no-op for an id that was evicted or already
    /// collected: the callback fires exactly once and must not resurrect a slot
    /// nobody is waiting on.
    pub fn complete(&self, id: &str, answer: String) {
        let mut slots = self.slots.lock().unwrap();
        if let Some(slot) = slots.by_id.get_mut(id) {
            *slot = Some(answer);
        }
    }

    /// Collect `id`. A [`Job::Ready`] is handed over ONCE — the slot is freed,
    /// so a poller that keeps asking gets `Unknown` rather than the same answer
    /// for ever, and the board does not need a second call to clear it.
    pub fn take(&self, id: &str) -> Job {
        let mut slots = self.slots.lock().unwrap();
        let Some(slot) = slots.by_id.get_mut(id) else { return Job::Unknown };
        let Some(answer) = slot.take() else { return Job::Pending };
        slots.by_id.remove(id);
        slots.order.retain(|x| x != id);
        Job::Ready(answer)
    }

    /// Slots currently held (in flight + completed-and-uncollected).
    pub fn len(&self) -> usize {
        self.slots.lock().unwrap().order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Slots {
    const fn new() -> Self {
        Self { by_id: BTreeMap::new(), order: VecDeque::new() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_job_is_pending() {
        let board = JobBoard::new();
        let id = board.start();
        assert_eq!(board.take(&id), Job::Pending);
    }

    #[test]
    fn ids_are_distinct() {
        let board = JobBoard::new();
        let a = board.start();
        let b = board.start();
        assert_ne!(a, b);
    }

    #[test]
    fn a_completed_job_reads_ready_once_and_then_unknown() {
        let board = JobBoard::new();
        let id = board.start();
        board.complete(&id, "{\"ok\":true}".to_string());
        assert_eq!(board.take(&id), Job::Ready("{\"ok\":true}".to_string()));
        assert_eq!(board.take(&id), Job::Unknown);
    }

    #[test]
    fn an_id_that_was_never_issued_is_unknown() {
        let board = JobBoard::new();
        assert_eq!(board.take("j999"), Job::Unknown);
    }

    #[test]
    fn collecting_frees_the_slot() {
        let board = JobBoard::new();
        let id = board.start();
        assert_eq!(board.len(), 1);
        board.complete(&id, "x".to_string());
        let _ = board.take(&id);
        assert!(board.is_empty());
    }

    // The bound is the point: a caller that starts and never collects must not
    // grow the board. The oldest slot goes, and it reads Unknown — an answer a
    // caller can act on — rather than staying Pending for ever.
    #[test]
    fn the_board_is_bounded_and_evicts_the_oldest() {
        let board = JobBoard::new();
        let first = board.start();
        for _ in 1..CAPACITY {
            board.start();
        }
        assert_eq!(board.len(), CAPACITY);
        let newest = board.start();
        assert_eq!(board.len(), CAPACITY);
        assert_eq!(board.take(&first), Job::Unknown);
        assert_eq!(board.take(&newest), Job::Pending);
    }

    // A reply that lands after its slot was evicted is dropped, not resurrected:
    // the callback fires exactly once and nothing is waiting on that id.
    #[test]
    fn completing_an_evicted_job_does_not_resurrect_it() {
        let board = JobBoard::new();
        let first = board.start();
        for _ in 0..CAPACITY {
            board.start();
        }
        board.complete(&first, "late".to_string());
        assert_eq!(board.take(&first), Job::Unknown);
    }
}
