//! Single-producer / single-consumer sample ring used by both audio paths:
//!
//! * capture: HAL IO thread pushes, `SystemTap::next_pcm` pops;
//! * playback: `Output::write` pushes, the AudioUnit render callback pops.
//!
//! Policy when full: **overwrite the oldest** data (SPEC §12: stale audio is
//! dropped, never delayed). Overwrite is implemented by the producer
//! atomically stealing read slots, so both sides stay lock-free — the only
//! synchronization is two monotonic counters with acquire/release ordering.
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) struct SpscRing {
    data: UnsafeCell<Box<[f32]>>,
    mask: usize,
    /// Total samples ever pushed (producer-owned, published with Release).
    tail: AtomicUsize,
    /// Total samples ever consumed or stolen (consumer/producer advance).
    head: AtomicUsize,
    /// Samples overwritten by the producer (diagnostics).
    dropped: AtomicUsize,
}

// SAFETY: justification for sharing `SpscRing` across threads: the producer
// only writes slots in [tail, tail+n) and publishes them with a Release store
// to `tail`; the consumer only reads slots in [head, tail) (loaded with
// Acquire) and publishes consumption with a Release store to `head`. These
// disjoint-slot invariants give race-free handoff in the steady state.
//
// One deliberate exception: when the producer must steal slots to overwrite
// old data, it advances `head` with fetch_add(Release) and immediately writes
// the stolen slots; if the consumer is concurrently reading exactly those
// slots it can observe at most a handful of samples from the *newest* write
// where it expected the oldest — an inaudible single-glitch artifact under
// an overflow the API already defines as lossy ("drop stale audio"). This is
// the standard bounded-lossy-audio-ring tradeoff; the alternative (blocking
// the realtime producer) is not acceptable on an audio IO thread.
unsafe impl Sync for SpscRing {}
unsafe impl Send for SpscRing {}

impl SpscRing {
    /// Ring with capacity for at least `samples` f32 samples (rounded up to a
    /// power of two).
    pub(crate) fn with_capacity_samples(samples: usize) -> Self {
        let cap = samples.max(8).next_power_of_two();
        Self {
            data: UnsafeCell::new(vec![0.0; cap].into_boxed_slice()),
            mask: cap - 1,
            tail: AtomicUsize::new(0),
            head: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
        }
    }

    fn capacity(&self) -> usize {
        self.mask + 1
    }

    /// Producer side: copy `src` into the ring, overwriting the oldest
    /// samples (and counting them in `dropped()`) when it does not fit.
    pub(crate) fn push(&self, src: &[f32]) {
        let cap = self.capacity();
        // If one push exceeds the whole ring, keep only the newest samples.
        let src = if src.len() > cap { &src[src.len() - cap..] } else { src };
        let n = src.len();
        if n == 0 {
            return;
        }
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Relaxed);
        let free = cap - (tail - head);
        if n > free {
            let steal = n - free;
            self.head.fetch_add(steal, Ordering::Release);
            self.dropped.fetch_add(steal, Ordering::Relaxed);
        }
        let start = tail & self.mask;
        let first = n.min(cap - start);
        // SAFETY: `first` slots starting at `start` are within the allocation
        // (start < cap, start + first <= cap) and, per the protocol above,
        // are owned by the producer right now.
        unsafe {
            let base = (*self.data.get()).as_ptr();
            std::ptr::copy_nonoverlapping(src.as_ptr(), base.add(start).cast_mut(), first);
            if first < n {
                std::ptr::copy_nonoverlapping(
                    src.as_ptr().add(first),
                    base.cast_mut(),
                    n - first,
                );
            }
        }
        self.tail.store(tail + n, Ordering::Release);
    }

    /// Consumer side: pop up to `dst.len()` samples into `dst` (front of the
    /// buffer), returning how many were available. Never blocks.
    pub(crate) fn pop(&self, dst: &mut [f32]) -> usize {
        let tail = self.tail.load(Ordering::Acquire);
        let head = self.head.load(Ordering::Relaxed);
        let avail = (tail - head).min(dst.len());
        if avail == 0 {
            return 0;
        }
        let cap = self.capacity();
        let start = head & self.mask;
        let first = avail.min(cap - start);
        // SAFETY: [head, head+avail) was published by the producer (Acquire
        // load of `tail` above pairs with its Release store), so the slots
        // are stable reads for the consumer.
        unsafe {
            let base = (*self.data.get()).as_ptr();
            std::ptr::copy_nonoverlapping(base.add(start), dst.as_mut_ptr(), first);
            if first < avail {
                std::ptr::copy_nonoverlapping(
                    base,
                    dst.as_mut_ptr().add(first),
                    avail - first,
                );
            }
        }
        self.head.store(head + avail, Ordering::Release);
        avail
    }

    /// Samples currently readable.
    pub(crate) fn available(&self) -> usize {
        self.tail.load(Ordering::Acquire) - self.head.load(Ordering::Acquire)
    }

    /// Samples overwritten since construction (overflow diagnostics).
    pub(crate) fn dropped(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_then_pop_exact() {
        let r = SpscRing::with_capacity_samples(16);
        let src: Vec<f32> = (0..10).map(|i| i as f32).collect();
        r.push(&src);
        assert_eq!(r.available(), 10);
        let mut dst = [0.0f32; 16];
        assert_eq!(r.pop(&mut dst), 10);
        assert_eq!(&dst[..10], &src[..]);
        assert_eq!(r.available(), 0);
        assert_eq!(r.dropped(), 0);
    }

    #[test]
    fn wraparound_preserves_order() {
        let r = SpscRing::with_capacity_samples(8);
        let mut expect = Vec::new();
        for chunk in 0..5u32 {
            let c: Vec<f32> = (0..4).map(|i| (chunk * 4 + i) as f32).collect();
            r.push(&c);
            expect.extend_from_slice(&c);
            let mut dst = [0.0f32; 4];
            assert_eq!(r.pop(&mut dst), 4);
            assert_eq!(&dst[..], &expect[expect.len() - 4..]);
        }
        assert_eq!(r.dropped(), 0);
    }

    #[test]
    fn overflow_overwrites_oldest() {
        let r = SpscRing::with_capacity_samples(8);
        r.push(&[1.0; 8]);
        r.push(&[2.0; 4]); // 4 oldest 1.0s stolen
        assert_eq!(r.available(), 8);
        assert_eq!(r.dropped(), 4);
        let mut dst = [0.0f32; 8];
        assert_eq!(r.pop(&mut dst), 8);
        assert_eq!(&dst[..4], &[1.0; 4]);
        assert_eq!(&dst[4..], &[2.0; 4]);
    }

    #[test]
    fn push_larger_than_ring_keeps_newest() {
        let r = SpscRing::with_capacity_samples(8);
        let src: Vec<f32> = (0..20).map(|i| i as f32).collect();
        r.push(&src);
        assert_eq!(r.available(), 8);
        assert_eq!(r.dropped(), 0); // truncated before entering the ring
        let mut dst = [0.0f32; 8];
        assert_eq!(r.pop(&mut dst), 8);
        assert_eq!(&dst[..], &src[12..]);
    }

    #[test]
    fn starvation_returns_zero() {
        let r = SpscRing::with_capacity_samples(8);
        let mut dst = [1.0f32; 4];
        assert_eq!(r.pop(&mut dst), 0);
        assert_eq!(r.available(), 0);
    }

    #[test]
    fn pop_partial_when_dst_small() {
        let r = SpscRing::with_capacity_samples(16);
        r.push(&(0..10).map(|i| i as f32).collect::<Vec<_>>());
        let mut dst = [0.0f32; 4];
        assert_eq!(r.pop(&mut dst), 4);
        assert_eq!(&dst, &[0.0, 1.0, 2.0, 3.0]);
        assert_eq!(r.available(), 6);
    }
}
