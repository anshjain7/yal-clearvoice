//! Sarvam's batch speech-to-text, for the after-call pass (aftercall.rs):
//! create a job, upload its WAV files to the links Sarvam returns, start
//! it, wait for it, then download each file's result.
//! https://docs.sarvam.ai/api-reference/speech-to-text/stt/job/initiate
//!
//! Sarvam's timestamps are coarse (a single chunk for a short file), so the
//! caller sends one utterance per file and keeps its own times.
//!
//! The Sarvam key goes to api.sarvam.ai only; the upload and download links
//! are signed by Sarvam and get no key.

use std::collections::HashMap;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::https;

const API: &str = "https://api.sarvam.ai/speech-to-text/job/v1";
/// Most files Sarvam takes in one job.
const FILES_PER_JOB: usize = 20;
/// How often a job's state is checked.
const POLL: Duration = Duration::from_secs(3);

/// The text of each of `wavs` (16 kHz WAV files' bytes), in order, via
/// Sarvam's batch service with `key`, in `language` (a Sarvam code, or
/// "auto"); "" for a file Sarvam heard nothing in. Each job gives up after
/// `wait`.
pub fn transcribe(
    key: &str,
    language: &str,
    wavs: &[Vec<u8>],
    wait: Duration,
) -> Result<Vec<String>, String> {
    let mut texts = Vec::with_capacity(wavs.len());
    for (n, group) in wavs.chunks(FILES_PER_JOB).enumerate() {
        texts.extend(job(key, language, group, n * FILES_PER_JOB, wait)?);
    }
    Ok(texts)
}

/// One job of up to FILES_PER_JOB files, named from `first` on.
fn job(
    key: &str,
    language: &str,
    wavs: &[Vec<u8>],
    first: usize,
    wait: Duration,
) -> Result<Vec<String>, String> {
    let api = |method: &str, path: &str, body: &Value| -> Result<Value, String> {
        let url = format!("{API}{path}");
        let body = if body.is_null() {
            Vec::new()
        } else {
            body.to_string().into_bytes()
        };
        let headers = [
            ("api-subscription-key", key),
            ("Content-Type", "application/json"),
        ];
        let r = https::request(method, &url, &headers, &body)?;
        let text = String::from_utf8_lossy(&r.body).to_string();
        if !(200..300).contains(&r.status) {
            return Err(format!(
                "Sarvam said {} to {method} {path}: {text}",
                r.status
            ));
        }
        serde_json::from_str(&text).map_err(|e| format!("Sarvam's reply to {path}: {e}"))
    };
    let names: Vec<String> = (first..first + wavs.len())
        .map(|i| format!("u{i:04}.wav"))
        .collect();
    let language = if language == "auto" {
        "unknown"
    } else {
        language
    };
    let job = api(
        "POST",
        "",
        &json!({ "job_parameters": {
            "model": "saaras:v3",
            "mode": "transcribe",
            "language_code": language,
            "input_audio_codec": "wav",
        }}),
    )?;
    let id = job["job_id"]
        .as_str()
        .ok_or("no job_id from Sarvam")?
        .to_string();

    let up = api(
        "POST",
        "/upload-files",
        &json!({ "job_id": id, "files": names }),
    )?;
    let azure = up["storage_container_type"]
        .as_str()
        .is_some_and(|t| t.starts_with("Azure"));
    let mut headers = vec![("Content-Type", "audio/wav")];
    if azure {
        headers.push(("x-ms-blob-type", "BlockBlob"));
    }
    for (name, wav) in names.iter().zip(wavs) {
        let link = up["upload_urls"][name]["file_url"]
            .as_str()
            .ok_or("no upload link from Sarvam")?;
        let put = https::request("PUT", link, &headers, wav)?;
        if !(200..300).contains(&put.status) {
            return Err(format!("the upload was refused ({})", put.status));
        }
    }

    api("POST", &format!("/{id}/start"), &Value::Null)?;
    let started = Instant::now();
    let status = loop {
        thread::sleep(POLL);
        let s = api("GET", &format!("/{id}/status"), &Value::Null)?;
        match s["job_state"].as_str() {
            Some("Completed") => break s,
            Some("Failed") => {
                return Err(format!("Sarvam's job failed: {}", s["error_message"]));
            }
            _ if started.elapsed() > wait => return Err("Sarvam's job took too long".into()),
            _ => {}
        }
    };
    let outputs = outputs_by_input(&status);
    let files: Vec<&String> = outputs.values().collect();
    let mut texts: HashMap<String, String> = HashMap::new();
    if !files.is_empty() {
        let down = api(
            "POST",
            "/download-files",
            &json!({ "job_id": id, "files": files }),
        )?;
        for (input, output) in &outputs {
            let link = down["download_urls"][output]["file_url"]
                .as_str()
                .ok_or("no download link from Sarvam")?;
            let r = https::request("GET", link, &[], &[])?;
            if r.status != 200 {
                return Err(format!("the result download was refused ({})", r.status));
            }
            let result: Value = serde_json::from_slice(&r.body).map_err(|e| e.to_string())?;
            texts.insert(input.clone(), transcript_of(&result));
        }
    }
    Ok(names
        .iter()
        .map(|n| texts.remove(n).unwrap_or_default())
        .collect())
}

/// Each uploaded file's result file, from a finished job's status.
fn outputs_by_input(status: &Value) -> HashMap<String, String> {
    status["job_details"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| {
            let input = d["inputs"][0]["file_name"].as_str()?;
            let output = d["outputs"][0]["file_name"].as_str()?;
            Some((input.to_string(), output.to_string()))
        })
        .collect()
}

/// A result file's text.
fn transcript_of(result: &Value) -> String {
    result["transcript"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finished_job_maps_each_file_to_its_result() {
        let status = json!({
            "job_state": "Completed",
            "job_details": [
                { "inputs": [{ "file_name": "u0001.wav" }],
                  "outputs": [{ "file_name": "1.json" }], "state": "Success" },
                { "inputs": [{ "file_name": "u0000.wav" }],
                  "outputs": [{ "file_name": "0.json" }], "state": "Success" },
                { "inputs": [{ "file_name": "u0002.wav" }], "outputs": [],
                  "state": "API Error" }
            ]
        });
        let map = outputs_by_input(&status);
        assert_eq!(map.len(), 2);
        assert_eq!(
            (map["u0000.wav"].as_str(), map["u0001.wav"].as_str()),
            ("0.json", "1.json")
        );
    }

    #[test]
    fn results_give_their_transcript() {
        // As Sarvam returned it: the timestamps cover the whole file.
        let result = json!({
            "language_code": "en-IN",
            "timestamps": { "start_time_seconds": [0.0], "end_time_seconds": [18.52],
                            "words": ["My name is Anshan."] },
            "transcript": " My name is Anshan. "
        });
        assert_eq!(transcript_of(&result), "My name is Anshan.");
        assert_eq!(transcript_of(&json!({})), "");
    }

    /// Against Sarvam itself: a real job (it costs a little), on two parts
    /// of a recording. Set YAL_SARVAM_TEST to "<settings.ini with the
    /// key>|<16 kHz mono WAV>".
    /// cargo test --release transcribes_with_sarvam -- --ignored --nocapture
    #[test]
    #[ignore = "uses Sarvam's paid API"]
    fn transcribes_with_sarvam() {
        let spec = std::env::var("YAL_SARVAM_TEST").expect("set YAL_SARVAM_TEST");
        let (ini, wav) = spec.split_once('|').expect("<settings.ini>|<wav>");
        let settings = crate::transcribe::Settings::parse(&std::fs::read_to_string(ini).unwrap())
            .expect("a Sarvam key in settings.ini");
        let b = std::fs::read(wav).unwrap();
        let samples: Vec<i16> = b[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| i16::from_le_bytes(c))
            .collect();
        let half = samples.len() / 2;
        let wavs = [
            crate::record::wav_bytes(&samples[..half]),
            crate::record::wav_bytes(&samples[half..]),
        ];
        let started = Instant::now();
        let texts = transcribe(
            &settings.api_key,
            &settings.language,
            &wavs,
            Duration::from_secs(600),
        )
        .unwrap();
        println!("{:.0} s", started.elapsed().as_secs_f32());
        for t in &texts {
            println!("- {t}");
        }
        assert_eq!(texts.len(), 2);
        assert!(texts.iter().all(|t| !t.is_empty()));
    }
}
