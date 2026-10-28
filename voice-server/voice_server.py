"""YAL ClearVoice voice server: target speaker extraction, live and for transcripts.

The YAL ClearVoice app sends each agent's voice sample once (POST /enroll).
This server keeps only the agent's voice (WeSep's BSRNN target speaker
extraction, as run_tse.sh does) in two ways:

- Live, for the call (the stream port): the app streams the agent's
  microphone; every hop (0.32 s) the server runs WeSep on a window of
  [history (2.0 s) + hop + lookahead (0.2 s)] and sends back only the hop,
  crossfaded with the piece before. The customer hears the agent about
  hop + lookahead + compute + network later.
- For transcripts (POST /extract): 2 s at a time with 1 s of context.

Audio on the wire is 16 kHz mono 16-bit little-endian PCM.

  POST /enroll                       body: a WAV file (the voice sample)
       -> 200 {"id": "<id>"}
  POST /extract?id=<id>&context=<samples>&remix=<0..1>
                                     body: PCM, context first
       -> 200 PCM for the samples after the context
       -> 404 when the id is unknown (the app then enrolls again)
  GET  /health -> 200 {"ok": true, ...}

  Stream port (TCP): the app sends one JSON line
       {"key": "...", "id": "<id>", "context": <samples>, "hop": <samples>,
        "lookahead": <samples>, "remix": <0..1>}
  -> the server answers {"ok": true} or {"error": "..."} on one line (the
     first stream for a voice sample first measures WeSep's gain on it); then
     the app sends PCM as it's recorded and the server sends PCM back, hop
     samples at a time, the first for the stream's first hop.

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
import socket
import socketserver
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
# A stream's opening line, at most.
HANDSHAKE_MAX = 4096
# Each hop's piece is crossfaded with the one before over this much (20 ms).
FADE = SR // 50
# A stream that sends nothing for this long is closed.
STREAM_IDLE_S = 30


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
        self.voices = {}  # id -> the voice sample (float32), for gain()
        self.gains = {}  # id -> gain() for streams
        self.lock = threading.Lock()
        self.served = 0
        self.busy_s = 0.0
        # Live streams: open now, windows run, and their time.
        self.streams = 0
        self.windows = 0
        self.window_s = 0.0

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
                    old = next(iter(self.enrollments))
                    for d in (self.enrollments, self.voices, self.gains):
                        d.pop(old, None)
                self.enrollments[vid] = self.engine.enroll(voice)
                self.voices[vid] = voice
        return vid

    def gain(self, vid):
        """What WeSep's output is multiplied by for a stream: its level and
        sign on the agent's own voice sample (the agent alone), turned into
        the input's level, the right way up. Fixed for the stream, so a
        stretch where WeSep turns another voice down stays down; scaling each
        window to its input's loudness (as /extract does) would bring it back
        up. Measured once per voice sample."""
        if vid not in self.gains:
            voice = self.voices[vid][: 10 * SR]
            with self.lock:
                est = self.engine.extract(self.enrollments[vid], voice)
            rms = lambda x: float(np.sqrt(np.mean(x ** 2))) + 1e-8  # noqa: E731
            sign = 1.0 if float(np.dot(est, voice)) >= 0 else -1.0
            self.gains[vid] = sign * rms(voice) / rms(est)
        return self.gains[vid]

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

    def window(self, enrollment, gain, mix, amount):
        """WeSep on one stream window (float32) at the stream's fixed `gain`,
        with `amount` of the original mixed back in; the same length."""
        t0 = time.time()
        with self.lock:
            est = self.engine.extract(enrollment, mix)
        self.windows += 1
        self.window_s += time.time() - t0
        return (1 - amount) * est * gain + amount * mix


class Stream:
    """One agent's live microphone, cleaned a hop at a time.

    Every hop, once the lookahead after it has arrived, WeSep runs on the
    window [history + hop + lookahead] and only the hop is sent back: the
    model is weakest at the end of its input, which the lookahead keeps away
    from. The first FADE samples of each hop are crossfaded from what the
    window before made of them."""

    def __init__(self, server, vid, context, hop, lookahead, amount):
        self.server, self.enrollment = server, server.enrollments[vid]
        self.gain = server.gain(vid)
        self.context, self.hop, self.lookahead, self.amount = context, hop, lookahead, amount
        self.fade = min(FADE, lookahead)
        self.audio = np.zeros(0, np.float32)  # kept audio; audio[0] is sample `base`
        self.base = 0
        self.next = 0  # the next hop to send starts here
        self.tail = None  # the last window's version of the next hop's first `fade`

    def add(self, samples):
        """Takes newly recorded samples; returns the hops now cleaned (PCM bytes)."""
        self.audio = np.concatenate([self.audio, samples])
        pieces = []
        while self.base + len(self.audio) >= self.next + self.hop + self.lookahead:
            start = max(self.base, self.next - self.context)
            end = self.next + self.hop + self.lookahead
            est = self.server.window(self.enrollment, self.gain,
                                     self.audio[start - self.base:end - self.base], self.amount)
            at = self.next - start
            piece = est[at:at + self.hop + self.fade].copy()
            if self.tail is not None and self.fade:
                w = (np.arange(self.fade, dtype=np.float32) + 0.5) / self.fade
                piece[:self.fade] = self.tail * (1 - w) + piece[:self.fade] * w
            self.tail = piece[self.hop:self.hop + self.fade].copy()
            pieces.append((np.clip(piece[:self.hop], -1, 1) * 32767).astype("<i2").tobytes())
            self.next += self.hop
            keep = self.next - self.context
            if keep > self.base:
                self.audio = self.audio[keep - self.base:]
                self.base = keep
        return pieces


def stream_handler(server):
    class Handler(socketserver.BaseRequestHandler):
        def handle(self):
            sock = self.request
            sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            sock.settimeout(STREAM_IDLE_S)
            reader = sock.makefile("rb")
            counted = False

            def refuse(error):
                sock.sendall((json.dumps({"error": error}) + "\n").encode())

            try:
                h = json.loads(reader.readline(HANDSHAKE_MAX) or b"{}")
                if server.api_key and h.get("key") != server.api_key:
                    return refuse("bad key")
                vid = str(h.get("id", ""))
                if vid not in server.enrollments:
                    return refuse("unknown voice sample")
                context = int(h.get("context", 2.0 * SR))
                hop = int(h.get("hop", 0.32 * SR))
                lookahead = int(h.get("lookahead", 0.2 * SR))
                amount = min(max(float(h.get("remix", 0.0)), 0.0), 1.0)
                if not (0 <= context <= 10 * SR and SR // 20 <= hop <= 2 * SR
                        and 0 <= lookahead <= SR):
                    return refuse("context, hop or lookahead out of range")
                stream = Stream(server, vid, context, hop, lookahead, amount)
                sock.sendall(b'{"ok": true}\n')
                server.streams += 1
                counted = True
                left = b""
                while True:
                    data = reader.read1(65536)
                    if not data:
                        return
                    data = left + data
                    whole = len(data) // 2 * 2
                    left = data[whole:]
                    samples = np.frombuffer(data[:whole], dtype="<i2").astype(np.float32) / 32768.0
                    for piece in stream.add(samples):
                        sock.sendall(piece)
            except (OSError, ValueError, RuntimeError):
                return  # the app reconnects; nothing is logged (call audio)
            finally:
                if counted:
                    server.streams -= 1

    return Handler


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
                win = server.window_s / server.windows if server.windows else 0
                self.reply(200, json.dumps({"ok": True, "served": server.served,
                                            "avg_ms": round(avg * 1000, 1),
                                            "streams": server.streams,
                                            "windows": server.windows,
                                            "window_avg_ms": round(win * 1000, 1)}).encode())
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
    p.add_argument("--stream-port", type=int, default=8091,
                   help="TCP port for live streams (0: none)")
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
    if a.stream_port:
        if a.script:
            p.error("live streams need the model loaded (--model-dir), not --script")
        socketserver.ThreadingTCPServer.allow_reuse_address = True
        socketserver.ThreadingTCPServer.daemon_threads = True
        streams = socketserver.ThreadingTCPServer((a.host, a.stream_port), stream_handler(server))
        threading.Thread(target=streams.serve_forever, daemon=True).start()
    httpd = ThreadingHTTPServer((a.host, a.port), handler(server))
    print(f"YAL ClearVoice voice server on http://{a.host}:{a.port}"
          f"{f', live streams on port {a.stream_port}' if a.stream_port else ''} "
          f"({'run_tse.sh' if a.script else a.device}"
          f"{', key required' if a.api_key else ''})", flush=True)
    httpd.serve_forever()


if __name__ == "__main__":
    main()
