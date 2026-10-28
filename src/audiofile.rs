//! Reads an audio file the agent picked (WAV, MP3, M4A, WMA, FLAC: whatever
//! Windows itself plays) as 16 kHz mono, for a voice sample uploaded instead
//! of recorded; and writes 16 kHz mono audio as an M4A (AAC) file, for the
//! agent voice files. Media Foundation decodes and encodes. It's loaded only
//! here, so a PC without it (Windows "N" editions) still runs the rest of
//! the app.

use std::ffi::c_void;
use std::path::Path;
use std::ptr::null_mut;

use windows::Win32::Media::MediaFoundation::{
    IMFMediaBuffer, IMFMediaType, IMFSample, IMFSinkWriter, IMFSourceReader,
    MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_FIRST_AUDIO_STREAM,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_VERSION, MFAudioFormat_AAC, MFAudioFormat_Float,
    MFAudioFormat_PCM, MFMediaType_Audio, MFSTARTUP_LITE,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{Error, HRESULT, HSTRING, Interface, PCSTR, PCWSTR, s, w};

use crate::transcribe::RATE;

/// Only the start of a long file is read.
const MAX_SECONDS: usize = 120;

/// The file as 16 kHz mono samples.
pub fn read(path: &Path) -> Result<Vec<i16>, String> {
    let (rate, audio) = unsafe { decode(path) }.map_err(|e| e.message())?;
    let audio = resample(&audio, rate, RATE);
    Ok(audio
        .iter()
        .map(|&s| (s * 32768.0).clamp(-32768.0, 32767.0) as i16)
        .collect())
}

/// Decodes `path` to mono float samples at the file's own rate.
unsafe fn decode(path: &Path) -> windows::core::Result<(u32, Vec<f32>)> {
    type Startup = unsafe extern "system" fn(u32, u32) -> HRESULT;
    type CreateType = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;
    type CreateReader = unsafe extern "system" fn(PCWSTR, *mut c_void, *mut *mut c_void) -> HRESULT;
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let plat = LoadLibraryW(w!("mfplat.dll"))?;
        let readwrite = LoadLibraryW(w!("mfreadwrite.dll"))?;
        let find = |dll, name: PCSTR| GetProcAddress(dll, name).ok_or_else(Error::from_win32);
        let startup: Startup = std::mem::transmute(find(plat, s!("MFStartup"))?);
        let create_type: CreateType = std::mem::transmute(find(plat, s!("MFCreateMediaType"))?);
        let create_reader: CreateReader =
            std::mem::transmute(find(readwrite, s!("MFCreateSourceReaderFromURL"))?);
        startup(MF_VERSION, MFSTARTUP_LITE).ok()?;

        let url = HSTRING::from(path.as_os_str());
        let mut raw = null_mut();
        create_reader(PCWSTR(url.as_ptr()), null_mut(), &mut raw).ok()?;
        let reader = IMFSourceReader::from_raw(raw);
        create_type(&mut raw).ok()?;
        let float = IMFMediaType::from_raw(raw);
        float.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        float.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float)?;
        let audio = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(audio, true)?;
        reader.SetCurrentMediaType(audio, None, &float)?;
        let format = reader.GetCurrentMediaType(audio)?;
        let rate = format.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)?;
        let channels = format.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)?.max(1) as usize;

        let mut mono = Vec::new();
        while mono.len() < MAX_SECONDS * rate as usize {
            let (mut flags, mut sample) = (0, None);
            reader.ReadSample(audio, 0, None, Some(&mut flags), None, Some(&mut sample))?;
            if let Some(sample) = sample {
                let buffer = sample.ConvertToContiguousBuffer()?;
                let (mut p, mut len) = (null_mut(), 0);
                buffer.Lock(&mut p, None, Some(&mut len))?;
                let bytes = std::slice::from_raw_parts(p, len as usize);
                let frame = |f: &[u8]| {
                    f.as_chunks::<4>()
                        .0
                        .iter()
                        .map(|&b| f32::from_le_bytes(b))
                        .sum::<f32>()
                        / channels as f32
                };
                mono.extend(bytes.chunks_exact(4 * channels).map(frame));
                buffer.Unlock()?;
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
        }
        Ok((rate, mono))
    }
}

/// Windows' AAC encoder takes 44.1 or 48 kHz only.
const AAC_RATE: u32 = 48_000;
/// 96 kbit/s, the encoder's lowest: far more than speech needs.
const AAC_BYTES_PER_SECOND: u32 = 12_000;
/// Audio handed to the encoder at a time: 1 s at 16 kHz.
const ENCODE_BLOCK: usize = RATE as usize;

/// Writes 16 kHz mono `samples` to `path` as an M4A (AAC) file.
pub fn write_m4a(samples: &[i16], path: &Path) -> Result<(), String> {
    unsafe { encode(samples, path) }.map_err(|e| e.message())
}

/// Encodes through Media Foundation's sink writer: 16 kHz up to 48 kHz here
/// (three output samples per input, the same band-limited filter as for
/// reading), then AAC.
unsafe fn encode(samples: &[i16], path: &Path) -> windows::core::Result<()> {
    type Startup = unsafe extern "system" fn(u32, u32) -> HRESULT;
    type CreateType = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;
    type CreateWriter =
        unsafe extern "system" fn(PCWSTR, *mut c_void, *mut c_void, *mut *mut c_void) -> HRESULT;
    type CreateBuffer = unsafe extern "system" fn(u32, *mut *mut c_void) -> HRESULT;
    type CreateSample = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let plat = LoadLibraryW(w!("mfplat.dll"))?;
        let readwrite = LoadLibraryW(w!("mfreadwrite.dll"))?;
        let find = |dll, name: PCSTR| GetProcAddress(dll, name).ok_or_else(Error::from_win32);
        let startup: Startup = std::mem::transmute(find(plat, s!("MFStartup"))?);
        let create_type: CreateType = std::mem::transmute(find(plat, s!("MFCreateMediaType"))?);
        let create_buffer: CreateBuffer =
            std::mem::transmute(find(plat, s!("MFCreateMemoryBuffer"))?);
        let create_sample: CreateSample = std::mem::transmute(find(plat, s!("MFCreateSample"))?);
        let create_writer: CreateWriter =
            std::mem::transmute(find(readwrite, s!("MFCreateSinkWriterFromURL"))?);
        startup(MF_VERSION, MFSTARTUP_LITE).ok()?;

        let audio_type = |subtype, bits: u32, bytes_per_second| -> windows::core::Result<_> {
            let mut raw = null_mut();
            create_type(&mut raw).ok()?;
            let t = IMFMediaType::from_raw(raw);
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
            t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AAC_RATE)?;
            t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 1)?;
            t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, bits)?;
            t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bytes_per_second)?;
            if bits == 16 && *subtype == MFAudioFormat_PCM {
                t.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 2)?;
            }
            Ok(t)
        };
        let url = HSTRING::from(path.as_os_str());
        let mut raw = null_mut();
        create_writer(PCWSTR(url.as_ptr()), null_mut(), null_mut(), &mut raw).ok()?;
        let writer = IMFSinkWriter::from_raw(raw);
        let aac = audio_type(&MFAudioFormat_AAC, 16, AAC_BYTES_PER_SECOND)?;
        let stream = writer.AddStream(&aac)?;
        let pcm = audio_type(&MFAudioFormat_PCM, 16, AAC_RATE * 2)?;
        writer.SetInputMediaType(stream, &pcm, None)?;
        writer.BeginWriting()?;

        // Each block resampled with the 32 samples either side of it, so the
        // joins between blocks are seamless.
        let factor = (AAC_RATE / RATE) as usize;
        let pad = 32;
        let x: Vec<f32> = samples.iter().map(|&s| f32::from(s) / 32768.0).collect();
        let mut written = 0u64;
        for start in (0..x.len()).step_by(ENCODE_BLOCK) {
            let end = (start + ENCODE_BLOCK).min(x.len());
            let (a, b) = (start.saturating_sub(pad), (end + pad).min(x.len()));
            let up = resample(&x[a..b], RATE, AAC_RATE);
            let up = &up[(start - a) * factor..((end - a) * factor).min(up.len())];
            let bytes: Vec<u8> = up
                .iter()
                .flat_map(|&v| ((v * 32768.0).clamp(-32768.0, 32767.0) as i16).to_le_bytes())
                .collect();
            create_buffer(bytes.len() as u32, &mut raw).ok()?;
            let buffer = IMFMediaBuffer::from_raw(raw);
            let mut p = null_mut();
            buffer.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(bytes.len() as u32)?;
            create_sample(&mut raw).ok()?;
            let sample = IMFSample::from_raw(raw);
            sample.AddBuffer(&buffer)?;
            // Times in 100 ns units.
            let frames = up.len() as u64;
            sample.SetSampleTime((written * 10_000_000 / u64::from(AAC_RATE)) as i64)?;
            sample.SetSampleDuration((frames * 10_000_000 / u64::from(AAC_RATE)) as i64)?;
            writer.WriteSample(stream, &sample)?;
            written += frames;
        }
        writer.Finalize()
    }
}

/// Band-limited resampling (Hann-windowed sinc), plenty for a voice sample.
fn resample(x: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || x.is_empty() {
        return x.to_vec();
    }
    let step = f64::from(from) / f64::from(to);
    // Cut off a little below the lower of the two Nyquist rates.
    let cutoff = 0.95 * (1.0 / step).min(1.0);
    let half = (16.0 / cutoff).ceil() as i64; // kernel half-width, input samples
    let n = (x.len() as f64 / step) as usize;
    (0..n)
        .map(|i| {
            let t = i as f64 * step;
            let centre = t.floor() as i64;
            let mut sum = 0.0;
            for k in (centre - half + 1).max(0)..=(centre + half).min(x.len() as i64 - 1) {
                let d = t - k as f64;
                let sinc = if d == 0.0 {
                    1.0
                } else {
                    (std::f64::consts::PI * cutoff * d).sin() / (std::f64::consts::PI * cutoff * d)
                };
                let window = 0.5 + 0.5 * (std::f64::consts::PI * d / half as f64).cos();
                sum += f64::from(x[k as usize]) * cutoff * sinc * window;
            }
            sum as f32
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f64, rate: u32, seconds: f64) -> Vec<f32> {
        (0..(f64::from(rate) * seconds) as usize)
            .map(|i| {
                (0.5 * (2.0 * std::f64::consts::PI * hz * i as f64 / f64::from(rate)).sin()) as f32
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    #[test]
    fn resampling_keeps_speech_and_drops_what_16k_cant_hold() {
        for from in [8000, 22050, 44100, 48000] {
            let out = resample(&tone(1000.0, from, 1.0), from, 16000);
            assert_eq!(out.len(), 16000, "{from}");
            // A 1 kHz tone keeps its level (0.354 RMS), away from the edges.
            let level = rms(&out[1000..15000]);
            assert!((level - 0.354).abs() < 0.01, "{from}: {level}");
        }
        // 12 kHz can't exist at 16 kHz: filtered out, not folded down to 4 kHz.
        let out = resample(&tone(12000.0, 48000, 1.0), 48000, 16000);
        assert!(rms(&out[1000..15000]) < 0.01);
    }

    #[test]
    fn m4a_round_trip_keeps_length_and_level() {
        let path = std::env::temp_dir().join(format!("clearvoice-m4a-{}.m4a", std::process::id()));
        // 3.3 s of a 440 Hz tone, at half scale.
        let tone: Vec<i16> = tone(440.0, RATE, 3.3)
            .iter()
            .map(|&s| (s * 32767.0) as i16)
            .collect();
        write_m4a(&tone, &path).unwrap();
        let back = read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let secs = back.len() as f64 / f64::from(RATE);
        assert!((secs - 3.3).abs() < 0.1, "{secs:.3} s back");
        let f = |s: &[i16]| {
            s.iter()
                .map(|&v| f32::from(v) / 32768.0)
                .collect::<Vec<_>>()
        };
        let (a, b) = (rms(&f(&tone[8000..40000])), rms(&f(&back[8000..40000])));
        let db = 20.0 * (b / a).log10();
        assert!(db.abs() < 1.0, "level changed by {db:.2} dB");
    }

    /// Reads a WAV through Media Foundation. Uses the files in YAL_AUDIO_TEST
    /// ("file|file|..."); each should hold at least a second of audio.
    #[test]
    #[ignore]
    fn reads_audio_files() {
        let files = std::env::var("YAL_AUDIO_TEST").unwrap();
        for f in files.split('|') {
            let audio = read(Path::new(f)).unwrap();
            let peak = audio.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
            println!("{f}: {:.1} s, peak {peak}", audio.len() as f64 / 16000.0);
            assert!(audio.len() > 16000 && peak > 100, "{f}");
        }
    }
}
