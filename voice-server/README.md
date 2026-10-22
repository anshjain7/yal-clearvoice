# YAL ClearVoice voice server

Keeps other people's voices out of the agents' transcripts. In an office,
the background is mostly other agents talking. Noise removal keeps that,
because it is speech, and Sarvam then writes it down as the agent's words.
This server runs target speaker extraction (WeSep BSRNN, the same as
`run_tse.sh`) on the agent's side of each transcribed call. It uses the
agent's own 10-second voice sample, and only that voice goes on to Sarvam.

- **What it changes:** only the transcript. What the customer hears still
  comes straight from noise removal on the agent's PC. This model needs
  audio from after each moment too, so it can't clean live call audio.
- **When it's used:** only while an agent is transcribing a call. The app
  sends 2 s of audio at a time, with the 1 s before it for context, so the
  agent's lines appear about 2 s later.
- **If it's down:** the app transcribes the original audio and tries the
  server again 30 s later.

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
    --wesep wesep --wespeaker wespeaker --device cuda --port 8090
```

- **The model:** `--model-dir` is the folder `run_tse.sh` loads its model
  from. WeSep's own pretrained English model works too: download
  `bsrnn_ecapa_vox1.tar.gz` from
  `https://www.modelscope.cn/datasets/wenet/wesep_pretrained_models/resolve/master/bsrnn_ecapa_vox1.tar.gz`
  and unpack it.
- **Newer WeSep models:** if your model was trained with a newer WeSep (its
  `config.yaml` has no `model: tse_model: BSRNN`), check out that version
  instead. Alternatively, run your script for every block with
  `--script /path/to/run_tse.sh`. That's slower, since it loads the model
  each time, so use it for checking, not for live calls.
- **Remix:** `--remix` is set per agent PC in `settings.ini` and defaults to
  0.2, as in the command you use. The server mixes the original back in
  itself, at the same loudness and the right way up. This model can return
  the voice inverted, which would otherwise cancel part of it.
- **Keep it running:** run it as a service, for example with systemd, and
  open port 8090 to the agents' network only.

Check it:

```bash
curl http://<server>:8090/health      # {"ok": true, "served": 0, "avg_ms": 0}
```

`avg_ms` is the average time per 3 s request. It must stay well under
2000 ms, or transcripts fall behind. On this test PC's CPU (8 threads) it
was about 1500 ms; a GPU should take a few tens of ms.

## On each agent PC

Add this to `settings.ini` next to the app (or to the setup's
`setup-settings.ini` before building the setup):

```ini
[voice]
url = http://<server>:8090
api_key = <the same YAL_VOICE_KEY>
remix = 0.2
```

Then each agent clicks **Record my voice** once and talks for 10 seconds.

The audio travels over plain HTTP unless the server is put behind HTTPS
(`url = https://...`); call audio should stay on the company network.

## Protocol

All audio is 16 kHz mono 16-bit little-endian PCM.

| Request | Body | Reply |
|---|---|---|
| `POST /enroll` | the voice sample, as a WAV file | `{"id": "<id>"}` |
| `POST /extract?id=<id>&context=<samples>&remix=<0..1>` | PCM, context first | PCM for the samples after the context; 404 if the id is unknown (the app then enrolls again) |
| `GET /health` | | `{"ok": true, "served": n, "avg_ms": t}` |

With `YAL_VOICE_KEY` set, every POST needs `Authorization: Bearer <key>`.
Nothing is logged per request, because requests carry call audio.
