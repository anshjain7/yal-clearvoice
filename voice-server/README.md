# YAL ClearVoice voice server

Keeps other people's voices out of the agents' calls and transcripts. In an
office, the background is mostly other agents talking. Noise removal keeps
that, because it is speech. This server runs target speaker extraction
(WeSep BSRNN, the same as `run_tse.sh`) on the agent's microphone, with the
agent's own 20-second voice sample, so only the agent's voice goes on.

- **Live, for the call (the stream port):** the app streams the agent's
  microphone here as it's recorded, on a sliding window:

      |<------ left context (2.0 s) ------>|<- hop (0.32 s) ->|<- lookahead (0.2 s) ->|
      model input = the whole window; output = only the hop

  Every hop, once the lookahead after it has arrived, the server runs WeSep
  on the whole window and sends back only the hop, crossfaded with the one
  before. The model is bidirectional and weakest at the very end of its
  input; the lookahead keeps the hop away from it, and the history gives it
  the context it has offline. The customer and the transcript hear the
  agent about **0.67 s** late (hop + lookahead + 0.15 s for the network and
  the GPU). A hop that isn't back in time, or the server being down, plays
  the agent's microphone as recorded.
- **For transcripts only (`/extract`):** with `stream = off` on the agent
  PCs, the app sends 2 s at a time with 1 s of context, for the transcript
  alone.

## It needs a GPU

Each hop runs WeSep on a 2.5 s window, about 3 times a second per agent. A
CPU is far too slow for that: the development PC (i5-12450H, 4 threads)
took about 1 s per window, three times longer than a hop, so it cleaned
only about a third of the hops and the rest played as recorded. On a GPU a
window takes a few tens of ms, so one GPU serves several agents at once.
`/health` shows the time per window (`window_avg_ms`), which must stay well
under the hop (320 ms) with all the agents on calls. Add GPUs (one server
process each) as agents are added.

The app can run the same sliding window on the agent's own CPU instead (no
`[voice]` url), with the same result: only some hops cleaned.

## Run it

On the GPU server (Linux, Python 3.10+):

```bash
python -m venv venv && . venv/bin/activate
pip install torch torchaudio            # the build matching your CUDA
pip install -r requirements.txt
git clone https://github.com/wenet-e2e/wesep.git
git -C wesep checkout 258a571           # 2024-11: reads the BSRNN models of that time
git clone https://github.com/wenet-e2e/wespeaker.git
git -C wespeaker checkout e9bbf73
export YAL_VOICE_KEY=<a long random string>
python voice_server.py --model-dir <dir with config.yaml and avg_model.pt> \
    --wesep wesep --wespeaker wespeaker --device cuda --port 8090 --stream-port 8091
```

- **The model:** `--model-dir` is the folder `run_tse.sh` loads its model
  from. WeSep's own pretrained English model works too: download
  `bsrnn_ecapa_vox1.tar.gz` from
  `https://www.modelscope.cn/datasets/wenet/wesep_pretrained_models/resolve/master/bsrnn_ecapa_vox1.tar.gz`
  and unpack it.
- **Newer WeSep models:** if your model was trained with a newer WeSep (its
  `config.yaml` has no `model: tse_model: BSRNN`), check out that version
  instead. `--script /path/to/run_tse.sh` runs your script for every
  transcript block instead (slow, for checking; not for live streams).
- **Level:** WeSep's output comes out much quieter than its input (about
  15 dB on the team's recordings), and other voices quieter still. For live
  streams the server measures that once per voice sample, on the sample
  itself (the agent alone), and applies it as a fixed gain, so the agent
  comes out at their own level and other voices stay turned down. (Scaling
  each window to its input's loudness would bring them back up.)
- **Keep it running:** run it as a service, for example with systemd, and
  open ports 8090 and 8091 to the agents' network only.

Check it:

```bash
curl http://<server>:8090/health
# {"ok": true, "served": 0, "avg_ms": 0, "streams": 0, "windows": 0, "window_avg_ms": 0}
```

`streams` is the number of agents streaming now; `window_avg_ms` the
average time per live window.

## On each agent PC

Add this to `settings.ini` next to the app (or to the setup's
`setup-settings.ini` before building the setup):

```ini
[voice]
url = http://<server>:8090
api_key = <the same YAL_VOICE_KEY>
```

That's all for the live stream, with these defaults (change them only to
test): `context_ms = 2000`, `hop_ms = 320`, `lookahead_ms = 200`,
`budget_ms = 150` (time allowed for the network and the GPU; the delay is
hop + lookahead + budget), `stream_port = 8091`, `stream_remix = 0` (how
much of the original microphone to mix back in). `stream = off` uses the
server for transcripts only.

Then each agent clicks **Register my voice** once and talks for 20 seconds.
The window's **Voice filter (WeSep)** box is on from the start; it shows
"Server not reachable" when the app can't reach the stream port, and the
agent's voice then goes out as recorded.

**Privacy:** the agents' microphones go to this server for the whole time
the app runs with the filter on. The stream is plain TCP and the HTTP
requests plain HTTP unless the server is behind HTTPS (`url =
https://...`, which covers only `/enroll` and `/extract`): keep the server
on the company network, and clear this with your data protection process
(OneTrust) before turning it on for real calls.

## Protocol

All audio is 16 kHz mono 16-bit little-endian PCM.

| Request | Body | Reply |
|---|---|---|
| `POST /enroll` | the voice sample, as a WAV file | `{"id": "<id>"}` |
| `POST /extract?id=<id>&context=<samples>&remix=<0..1>` | PCM, context first | PCM for the samples after the context; 404 if the id is unknown (the app then enrolls again) |
| `GET /health` | | `{"ok": true, "served": n, "avg_ms": t, "streams": n, "windows": n, "window_avg_ms": t}` |

With `YAL_VOICE_KEY` set, every POST needs `Authorization: Bearer <key>`.

Live stream (TCP, `--stream-port`): the app sends one JSON line,
`{"key": "<key>", "id": "<id from /enroll>", "context": <samples>, "hop":
<samples>, "lookahead": <samples>, "remix": <0..1>}`. The server answers
`{"ok": true}` or `{"error": "..."}` on one line, then the app sends the
microphone as it's recorded and the server sends back `hop` samples at a
time: the first for the stream's first `hop` samples, and so on, each once
the `lookahead` after it has arrived. A stream idle for 30 s is closed.

Nothing is logged per request or stream, because they carry call audio.
