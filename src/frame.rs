//! Fixed-size audio block passed from capture through processing to playback.

use std::time::Instant;

use crate::format::AudioFormat;

const FRAMES: usize = AudioFormat::MIC.frames;
const MAX_CHANNELS: u16 = AudioFormat::SPEAKER.channels;
const CAPACITY: usize = FRAMES * MAX_CHANNELS as usize;

/// One 10 ms block of interleaved f32 samples, mono or stereo.
///
/// Samples are stored inline and the type is `Copy`, so it cannot own heap
/// memory: creating or moving a frame never allocates, it is a plain memcpy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioFrame {
    /// Block number set by capture, +1 per block. A gap means a block was dropped.
    pub seq: u64,
    /// When capture finished this block, for measuring latency downstream.
    pub captured: Option<Instant>,
    channels: u16,
    data: [f32; CAPACITY],
}

impl AudioFrame {
    /// A silent frame. Panics unless `channels` is 1 or 2.
    pub const fn new(channels: u16) -> Self {
        assert!(
            matches!(channels, 1..=MAX_CHANNELS),
            "AudioFrame supports mono or stereo"
        );
        Self {
            seq: 0,
            captured: None,
            channels,
            data: [0.0; CAPACITY],
        }
    }

    pub const fn channels(&self) -> u16 {
        self.channels
    }

    /// Samples across all channels: 480 mono, 960 stereo.
    pub const fn sample_count(&self) -> usize {
        FRAMES * self.channels as usize
    }

    /// Interleaved samples, `sample_count()` long.
    pub fn samples(&self) -> &[f32] {
        &self.data[..self.sample_count()]
    }

    pub fn samples_mut(&mut self) -> &mut [f32] {
        let n = self.sample_count();
        &mut self.data[..n]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_count_follows_channels() {
        for (channels, count) in [(1, 480), (2, 960)] {
            let f = AudioFrame::new(channels);
            assert_eq!(f.channels(), channels);
            assert_eq!(f.sample_count(), count);
            assert_eq!(f.samples().len(), count);
        }
    }

    #[test]
    #[should_panic(expected = "mono or stereo")]
    fn rejects_more_than_stereo() {
        AudioFrame::new(3);
    }

    #[test]
    fn crosses_threads_intact() {
        let mut frame = AudioFrame::new(2);
        frame.seq = 7;
        frame.samples_mut().fill(0.25);

        let received = std::thread::spawn(move || frame).join().unwrap();
        assert_eq!(received, frame);
    }
}
