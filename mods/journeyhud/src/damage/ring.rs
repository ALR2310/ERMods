//! A bounded buffer that a game hook fills and the frame task empties. A full one drops what comes,
//! never waits: nothing in a hook may lock or allocate.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

pub struct Ring<T, const N: usize> {
    next: AtomicUsize,
    dropped: AtomicUsize,
    slots: [Slot<T>; N],
}

struct Slot<T> {
    state: AtomicU8,
    value: UnsafeCell<MaybeUninit<(usize, T)>>,
}

// SAFETY: `state` hands a slot to one side at a time (0 free, 1 being written or read, 2 published).
unsafe impl<T: Send> Sync for Slot<T> {}

impl<T: Copy, const N: usize> Ring<T, N> {
    pub const fn new() -> Self {
        Self {
            next: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
            slots: [const {
                Slot { state: AtomicU8::new(0), value: UnsafeCell::new(MaybeUninit::uninit()) }
            }; N],
        }
    }

    pub fn push(&self, value: T) {
        let sequence = self.next.fetch_add(1, Ordering::Relaxed);
        let slot = &self.slots[sequence % N];
        if slot.state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // SAFETY: the compare-exchange reserved this slot until it is published.
        unsafe { (*slot.value.get()).write((sequence, value)) };
        slot.state.store(2, Ordering::Release);
    }

    /// What the hook collected, oldest first, and how many it had to drop.
    pub fn drain(&self) -> (Vec<T>, usize) {
        let mut found = Vec::new();
        for slot in &self.slots {
            if slot.state.compare_exchange(2, 1, Ordering::Acquire, Ordering::Relaxed).is_err() {
                continue;
            }
            // SAFETY: acquire observes the published write and the compare-exchange excludes other access.
            found.push(unsafe { (*slot.value.get()).assume_init_read() });
            slot.state.store(0, Ordering::Release);
        }
        found.sort_unstable_by_key(|(sequence, _)| *sequence);
        (
            found.into_iter().map(|(_, value)| value).collect(),
            self.dropped.swap(0, Ordering::Relaxed),
        )
    }
}
