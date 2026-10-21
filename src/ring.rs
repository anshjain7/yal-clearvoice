//! Wait-free single-producer, single-consumer ring of audio frames.

use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};

use crate::frame::AudioFrame;

struct Shared {
    slots: Box<[UnsafeCell<AudioFrame>]>,
    // ponytail: slot = counter % len assumes counters never wrap: 2^64 frames, ~5.8 billion years.
    /// Frames read so far. Only the consumer writes it.
    head: AtomicUsize,
    /// Frames written so far. Only the producer writes it.
    tail: AtomicUsize,
}

// SAFETY: there is exactly one Producer and one Consumer (neither is Clone and
// both take &mut self). The producer writes only slots outside head..tail, the
// consumer reads only slots inside it, and Acquire/Release on head and tail
// order those accesses, so no slot is touched by both threads at once.
unsafe impl Sync for Shared {}

/// Writing end, for the thread that produces frames.
pub struct Producer(Arc<Shared>);

/// Reading end, for the thread that consumes frames.
pub struct Consumer(Arc<Shared>);

/// A ring holding up to `capacity` frames, so it adds at most `capacity` × 10 ms
/// of latency. All allocation happens here; `push` and `pop` never allocate,
/// lock or wait.
pub fn new(capacity: usize) -> (Producer, Consumer) {
    assert!(capacity > 0, "ring capacity must be at least 1");
    let shared = Arc::new(Shared {
        slots: (0..capacity)
            .map(|_| UnsafeCell::new(AudioFrame::new(1)))
            .collect(),
        head: AtomicUsize::new(0),
        tail: AtomicUsize::new(0),
    });
    (Producer(Arc::clone(&shared)), Consumer(shared))
}

impl Producer {
    /// Queues a copy of `frame`. On overflow (ring full) the frame is dropped,
    /// queued frames are kept, and this returns false.
    pub fn push(&mut self, frame: &AudioFrame) -> bool {
        let s = &*self.0;
        let tail = s.tail.load(Relaxed);
        if tail.wrapping_sub(s.head.load(Acquire)) == s.slots.len() {
            return false;
        }
        // SAFETY: this slot is outside head..tail, so the consumer is done with it.
        unsafe { *s.slots[tail % s.slots.len()].get() = *frame };
        s.tail.store(tail.wrapping_add(1), Release);
        true
    }
}

impl Consumer {
    /// Frames waiting to be read.
    pub fn queued(&self) -> usize {
        let s = &*self.0;
        s.tail.load(Acquire).wrapping_sub(s.head.load(Relaxed))
    }

    /// The oldest queued frame, or `None` on underflow (ring empty).
    pub fn pop(&mut self) -> Option<AudioFrame> {
        let s = &*self.0;
        let head = s.head.load(Relaxed);
        if head == s.tail.load(Acquire) {
            return None;
        }
        // SAFETY: this slot is inside head..tail, so the producer has published
        // it and won't write it again until head moves past.
        let frame = unsafe { *s.slots[head % s.slots.len()].get() };
        s.head.store(head.wrapping_add(1), Release);
        Some(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::{Duration, Instant};

    /// Frames per stress run; Miri is far slower, so it gets a short run.
    const N: u64 = if cfg!(miri) { 100 } else { 20_000 };
    /// Extra time per frame for whichever side should be slower.
    const SLOW: Duration = Duration::from_micros(20);
    /// Waiting this long on the other side counts as a deadlock.
    const STALL: Duration = Duration::from_secs(5);

    /// Frame `seq`: mono when even, stereo when odd, every sample = `seq`.
    fn frame(seq: u64) -> AudioFrame {
        let mut f = AudioFrame::new(1 + (seq % 2) as u16);
        f.seq = seq;
        f.samples_mut().fill(seq as f32);
        f
    }

    fn busy_wait(d: Duration) {
        let start = Instant::now();
        while start.elapsed() < d {}
    }

    /// Streams `n` frames through a ring of `capacity`, producer and consumer
    /// on their own threads, each pausing its delay after every frame. The
    /// producer retries when full, so every frame must arrive intact and in
    /// order. Returns how often the ring was found full and found empty.
    fn stream(capacity: usize, n: u64, push_delay: Duration, pop_delay: Duration) -> (u64, u64) {
        let (mut p, mut c) = new(capacity);
        let producer = thread::spawn(move || {
            let mut full = 0;
            for seq in 0..n {
                let f = frame(seq);
                let start = Instant::now();
                while !p.push(&f) {
                    full += 1;
                    assert!(start.elapsed() < STALL, "producer stalled at frame {seq}");
                    thread::yield_now();
                }
                busy_wait(push_delay);
            }
            full
        });
        let mut empty = 0;
        for seq in 0..n {
            let start = Instant::now();
            let f = loop {
                if let Some(f) = c.pop() {
                    break f;
                }
                empty += 1;
                assert!(start.elapsed() < STALL, "consumer stalled at frame {seq}");
                thread::yield_now();
            };
            assert_eq!(f.seq, seq, "frame lost, duplicated or reordered");
            assert!(f == frame(seq), "frame {seq} corrupted");
            busy_wait(pop_delay);
        }
        let full = producer.join().unwrap();
        assert_eq!(c.pop(), None, "extra frame");
        (full, empty)
    }

    #[test]
    fn write_read() {
        let (mut p, mut c) = new(4);
        assert!(p.push(&frame(1)));
        assert!(p.push(&frame(2)));
        assert_eq!(c.queued(), 2);
        assert_eq!(c.pop(), Some(frame(1)));
        assert_eq!(c.pop(), Some(frame(2)));
        assert_eq!(c.queued(), 0);
    }

    #[test]
    fn wraparound() {
        let (mut p, mut c) = new(3);
        // Keep the ring full while the indices lap it several times.
        for seq in 0..20 {
            assert!(p.push(&frame(seq)));
            if seq >= 2 {
                assert_eq!(c.pop(), Some(frame(seq - 2)));
            }
        }
    }

    #[test]
    fn overflow_drops_new_frame_and_keeps_queue() {
        let (mut p, mut c) = new(2);
        assert!(p.push(&frame(0)));
        assert!(p.push(&frame(1)));
        assert!(!p.push(&frame(2)));
        assert_eq!(c.pop(), Some(frame(0)));
        assert!(p.push(&frame(3)));
        assert_eq!(c.pop(), Some(frame(1)));
        assert_eq!(c.pop(), Some(frame(3)));
    }

    #[test]
    fn underflow_returns_none() {
        let (mut p, mut c) = new(2);
        assert_eq!(c.pop(), None);
        assert!(p.push(&frame(0)));
        assert_eq!(c.pop(), Some(frame(0)));
        assert_eq!(c.pop(), None);
    }

    #[test]
    fn stress_rates() {
        let fast = Duration::ZERO;
        // Matched rates: continuous producer and consumer.
        stream(4, N, fast, fast);
        // Producer faster: the ring runs near capacity.
        let (full, _) = stream(4, N, fast, SLOW);
        assert!(full > 0, "ring never filled");
        // Consumer faster: the ring runs near empty.
        let (_, empty) = stream(4, N, SLOW, fast);
        assert!(empty > 0, "ring never ran empty");
    }

    #[test]
    fn stress_capacities() {
        for capacity in [1, 2, 3, 64] {
            stream(capacity, N, Duration::ZERO, Duration::ZERO);
        }
    }

    #[test]
    fn realtime_producer_drops_only_what_it_reports() {
        // Like a capture thread: push once, never wait, count overflows.
        let (mut p, mut c) = new(4);
        let producer =
            thread::spawn(move || (0..N).filter(|&seq| !p.push(&frame(seq))).count() as u64);
        let mut received = Vec::new();
        let mut drain = |c: &mut Consumer| {
            while let Some(f) = c.pop() {
                assert!(f == frame(f.seq), "frame {} corrupted", f.seq);
                received.push(f.seq);
            }
        };
        while !producer.is_finished() {
            drain(&mut c);
            busy_wait(SLOW);
        }
        let dropped = producer.join().unwrap();
        drain(&mut c);
        assert!(dropped > 0, "ring never overflowed");
        assert!(
            received.windows(2).all(|w| w[0] < w[1]),
            "frames reordered or duplicated"
        );
        assert_eq!(
            received.len() as u64 + dropped,
            N,
            "dropped frames went unreported"
        );
    }

    #[test]
    #[ignore = "long-running: cargo test --release -- --ignored --nocapture"]
    fn long_running() {
        let n = 2_000_000; // about 5.5 hours of audio
        let start = Instant::now();
        let (full, empty) = stream(4, n, Duration::ZERO, Duration::ZERO);
        let took = start.elapsed();
        println!(
            "{n} frames in {took:.2?} ({:.2?} per frame), full {full}x, empty {empty}x",
            took / n as u32
        );
    }
}
