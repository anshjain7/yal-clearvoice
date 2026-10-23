//! Reads an audio file the agent picked (WAV, MP3, M4A, WMA, FLAC: whatever
//! Windows itself plays) as 16 kHz mono, for a voice sample uploaded instead
//! of recorded. Media Foundation decodes it. It's loaded only here, so a PC
//! without it (Windows "N" editions) still runs the rest of the app.

use std::ffi::c_void;
use std::path::Path;
use std::ptr::null_mut;

use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, IMFSourceReader, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SOURCE_READER_ALL_STREAMS,
    MF_SOURCE_READER_FIRST_AUDIO_STREAM, MF_SOURCE_READERF_ENDOFSTREAM, MF_VERSION,
    MFAudioFormat_Float, MFMediaType_Audio, MFSTARTUP_LITE,
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
