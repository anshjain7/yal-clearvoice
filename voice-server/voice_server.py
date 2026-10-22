"""YAL ClearVoice voice server: target speaker extraction for transcripts.

The YAL ClearVoice app sends each agent's voice sample once (POST /enroll),
then, during transcribed calls, the agent's audio 2 s at a time with 1 s of
context before it (POST /extract). This server keeps only the agent's voice
(WeSep's BSRNN target speaker extraction, as run_tse.sh does), mixes part of
the original back in (run_tse.sh's --remix), and returns the cleaned audio
for the block, without the context.

Audio on the wire is 16 kHz mono 16-bit little-endian PCM.

  POST /enroll                       body: a WAV file (the voice sample)
       -> 200 {"id": "<id>"}
  POST /extract?id=<id>&context=<samples>&remix=<0..1>
                                     body: PCM, context first
       -> 200 PCM for the samples after the context
       -> 404 when the id is unknown (the app then enrolls again)
  GET  /health -> 200 {"ok": true, ...}

Run it (see README.md):
  python voice_server.py --model-dir <dir with config.yaml and avg_model.pt> \
      --wesep <wesep checkout> --wespeaker <wespeaker checkout> --device cuda
or, to run the team's own script for every block (slower, for checking):
  python voice_server.py --script /path/to/run_tse.sh
"""
import argparse
import hashlib
import io
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
import types
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

import numpy as np
import soundfile

SR = 16000
MAX_BODY = 30 * SR * 2  # 30 s of audio, far more than the app sends
ENROLLMENTS_KEPT = 1000


class Model:
    """WeSep's BSRNN target speaker extraction, loaded once."""

    def __init__(self, model_dir, device, threads):
        import torch
        import torchaudio.compliance.kaldi as kaldi
        import yaml
        from wesep.models import get_model
        from wesep.utils.checkpoint import load_pretrained_model

        self.torch, self.kaldi = torch, kaldi
        if threads:
            torch.set_num_threads(threads)
        with open(os.path.join(model_dir, "config.yaml")) as f:
            cfg = yaml.safe_load(f)
        args = cfg["model_args"]["tse_model"]
        args["spk_model_init"] = False  # the speaker model's weights are in avg_model.pt
        self.net = get_model(cfg["model"]["tse_model"])(**args)
        load_pretrained_model(self.net, os.path.join(model_dir, "avg_model.pt"))
        self.device = torch.device(device)
        self.net.to(self.device).eval()

    def enroll(self, voice):
        """Speaker features for a voice sample (float32, 16 kHz)."""
        pcm = self.torch.from_numpy(voice).unsqueeze(0)
        feats = self.kaldi.fbank(pcm, num_mel_bins=80, frame_length=25,
                                 frame_shift=10, sample_frequency=SR)
        return (feats - feats.mean(0)).unsqueeze(0).to(self.device)

    def extract(self, enrollment, mix):
        """The target voice in `mix` (float32), the same length."""
        with self.torch.no_grad():
            pcm = self.torch.from_numpy(mix).unsqueeze(0).to(self.device)
            out = self.net(pcm, enrollment)
            out = out[0] if isinstance(out, (list, tuple)) else out
        return out.reshape(-1)[: len(mix)].cpu().numpy().astype(np.float32)


class Script:
    """Runs the team's run_tse.sh for every block, exactly as on the command line."""

    def __init__(self, script):
        self.script = os.path.abspath(script)
        self.dir = tempfile.mkdtemp(prefix="yal-voice-")

    def enroll(self, voice):
        path = os.path.join(self.dir, f"enroll-{time.time_ns()}.wav")
        soundfile.write(path, voice, SR)
        return path

    def extract(self, enrollment, mix):
        with tempfile.TemporaryDirectory(dir=self.dir) as d:
            inp, out = os.path.join(d, "input.wav"), os.path.join(d, "out.wav")
            soundfile.write(inp, mix, SR)
            # --remix 0: the server mixes the original back in itself, the same
            # way for both modes.
            subprocess.run(["bash", self.script, "--enroll", enrollment, "--input", inp,
                            "--output", out, "--remix", "0"],
                           cwd=os.path.dirname(self.script), check=True,
                           stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            est, sr = soundfile.read(out, dtype="float32")
            if est.ndim > 1:
                est = est.mean(axis=1)
            if sr != SR:
                raise RuntimeError(f"run_tse.sh wrote {sr} Hz audio; expected {SR}")
        est = np.pad(est, (0, max(0, len(mix) - len(est))))[: len(mix)]
        return est.astype(np.float32)


def remix(est, mix, amount):
    """run_tse.sh's --remix: the extracted voice at the input's loudness, the
    right way up (extraction may return it inverted, and mixing an inverted
    voice back with the original would cancel part of it), with `amount` of
    the original mixed back in."""
    rms = lambda x: float(np.sqrt(np.mean(x ** 2))) + 1e-8  # noqa: E731
    est = est * (rms(mix) / rms(est))
    if float(np.dot(est, mix)) < 0:
        est = -est
    out = (1 - amount) * est + amount * mix
    peak = float(np.abs(out).max(initial=0))
    return out * (0.99 / peak) if peak > 0.99 else out


class Server:
    def __init__(self, engine, api_key):
        self.engine, self.api_key = engine, api_key
        self.enrollments = {}  # id -> engine enrollment
        self.lock = threading.Lock()
        self.served = 0
        self.busy_s = 0.0

    def enroll(self, wav_bytes):
        voice, sr = soundfile.read(io.BytesIO(wav_bytes), dtype="float32", always_2d=True)
        voice = voice.mean(axis=1)
        if sr != SR:
            raise ValueError(f"voice sample is {sr} Hz; send {SR} Hz")
        if len(voice) < SR:
            raise ValueError("voice sample is shorter than 1 s")
        vid = hashlib.sha256(wav_bytes).hexdigest()[:32]
        with self.lock:
            if vid not in self.enrollments:
                if len(self.enrollments) >= ENROLLMENTS_KEPT:
                    self.enrollments.pop(next(iter(self.enrollments)))
                self.enrollments[vid] = self.engine.enroll(voice)
        return vid

    def extract(self, vid, pcm_bytes, context, amount):
        enrollment = self.enrollments.get(vid)
        if enrollment is None:
            return None
        mix = np.frombuffer(pcm_bytes, dtype="<i2").astype(np.float32) / 32768.0
        context = max(0, min(context, len(mix)))
        t0 = time.time()
        with self.lock:
            est = self.engine.extract(enrollment, mix)
        out = remix(est, mix, amount)[context:]
        self.served += 1
        self.busy_s += time.time() - t0
        return (np.clip(out, -1, 1) * 32767).astype("<i2").tobytes()


def handler(server):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def reply(self, status, body=b"", ctype="application/json"):
            self.send_response(status)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(body)
            self.close_connection = True

        def authorized(self):
            if not server.api_key:
                return True
            return self.headers.get("Authorization", "") == f"Bearer {server.api_key}"

        def do_GET(self):
            if urlparse(self.path).path.rstrip("/").endswith("/health"):
                avg = server.busy_s / server.served if server.served else 0
                self.reply(200, json.dumps({"ok": True, "served": server.served,
                                            "avg_ms": round(avg * 1000, 1)}).encode())
            else:
                self.reply(404)

        def do_POST(self):
            if not self.authorized():
                return self.reply(401, b'{"error":"bad key"}')
            length = int(self.headers.get("Content-Length", "0"))
            if length <= 0 or length > MAX_BODY:
                return self.reply(413, b'{"error":"body too large or empty"}')
            body = self.rfile.read(length)
            url = urlparse(self.path)
            q = {k: v[0] for k, v in parse_qs(url.query).items()}
            try:
                if url.path.rstrip("/").endswith("/enroll"):
                    return self.reply(200, json.dumps({"id": server.enroll(body)}).encode())
                if url.path.rstrip("/").endswith("/extract"):
                    amount = min(max(float(q.get("remix", 0.2)), 0.0), 1.0)
                    out = server.extract(q.get("id", ""), body, int(q.get("context", 0)), amount)
                    if out is None:
                        return self.reply(404, b'{"error":"unknown voice sample"}')
                    return self.reply(200, out, "application/octet-stream")
                return self.reply(404)
            except (ValueError, RuntimeError, subprocess.CalledProcessError) as e:
                return self.reply(400, json.dumps({"error": str(e)}).encode())

        def log_message(self, fmt, *args):
            pass  # no per-request logging: requests carry call audio

    return Handler


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--host", default="0.0.0.0")
    p.add_argument("--port", type=int, default=8090)
    p.add_argument("--model-dir", help="directory with config.yaml and avg_model.pt")
    p.add_argument("--wesep", help="WeSep checkout matching the model (added to the path)")
    p.add_argument("--wespeaker", help="WeSpeaker checkout (added to the path)")
    p.add_argument("--device", default="cpu", help="cpu or cuda")
    p.add_argument("--threads", type=int, default=0, help="CPU threads (0: PyTorch's default)")
    p.add_argument("--script", help="run this run_tse.sh for every block instead")
    p.add_argument("--api-key", default=os.environ.get("YAL_VOICE_KEY", ""),
                   help="require 'Authorization: Bearer <key>' (or set YAL_VOICE_KEY)")
    a = p.parse_args()

    if a.script:
        engine = Script(a.script)
    else:
        if not a.model_dir:
            p.error("give --model-dir, or --script")
        sys.path[:0] = [x for x in (a.wesep, a.wespeaker) if x]
        # WeSpeaker's package import pulls in its diarization tools, which
        # extraction never uses.
        for name in ("umap", "hdbscan"):
            sys.modules.setdefault(name, types.ModuleType(name))
        engine = Model(a.model_dir, a.device, a.threads)

    server = Server(engine, a.api_key)
    httpd = ThreadingHTTPServer((a.host, a.port), handler(server))
    print(f"YAL ClearVoice voice server on http://{a.host}:{a.port} "
          f"({'run_tse.sh' if a.script else a.device}"
          f"{', key required' if a.api_key else ''})", flush=True)
    httpd.serve_forever()


if __name__ == "__main__":
    main()
