//! Audio format shared by capture, processing and playback.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    /// 32-bit float, the internal processing format.
    F32,
    /// 16-bit PCM.
    I16,
}

impl SampleFormat {
    /// Bytes per sample.
    pub const fn bytes(self) -> usize {
        match self {
            Self::F32 => 4,
            Self::I16 => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: SampleFormat,
    /// Frames per buffer. One frame holds one sample per channel.
    pub frames: usize,
}

impl AudioFormat {
    /// Microphone: 48 kHz mono f32, 480-frame (10 ms) buffers, DFN3's frame size.
    pub const MIC: Self = Self {
        sample_rate: 48_000,
        channels: 1,
        sample_format: SampleFormat::F32,
        frames: 480,
    };

    /// Speaker: 48 kHz stereo f32, 480-frame (10 ms) buffers.
    pub const SPEAKER: Self = Self {
        sample_rate: 48_000,
        channels: 2,
        sample_format: SampleFormat::F32,
        frames: 480,
    };

    /// Buffer size in interleaved samples.
    pub const fn samples(&self) -> usize {
        self.frames * self.channels as usize
    }

    /// Buffer size in bytes.
    pub const fn bytes(&self) -> usize {
        self.samples() * self.sample_format.bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mic_is_48k_mono_f32() {
        let f = AudioFormat::MIC;
        assert_eq!(
            (f.sample_rate, f.channels, f.sample_format),
            (48_000, 1, SampleFormat::F32)
        );
        assert_eq!((f.samples(), f.bytes()), (480, 1920));
    }

    #[test]
    fn speaker_is_48k_stereo_f32() {
        let f = AudioFormat::SPEAKER;
        assert_eq!(
            (f.sample_rate, f.channels, f.sample_format),
            (48_000, 2, SampleFormat::F32)
        );
        assert_eq!((f.samples(), f.bytes()), (960, 3840));
    }

    #[test]
    fn buffers_are_10_ms() {
        for f in [AudioFormat::MIC, AudioFormat::SPEAKER] {
            assert_eq!(f.frames * 100, f.sample_rate as usize);
        }
    }

    #[test]
    fn i16_buffer_is_half_of_f32() {
        let f = AudioFormat {
            sample_format: SampleFormat::I16,
            ..AudioFormat::SPEAKER
        };
        assert_eq!(f.bytes(), AudioFormat::SPEAKER.bytes() / 2);
    }
}
