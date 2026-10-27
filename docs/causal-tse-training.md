# Training a live (causal) WeSep model for YAL ClearVoice

YAL ClearVoice's live chain is:

    microphone -> TSE -> DeepFilterNet3 -> VB-CABLE -> customer and transcript

The TSE step needs a target speaker extraction model that only uses audio
it has already heard ("causal"), so it can run every 8 ms with about 30 ms of
delay. WeSep's public model (`bsrnn_ecapa_vox1`) looks ahead as well, so it
can only run after the call. WeSep's current code trains causal models; no
pretrained one is published. This is what to train.

## Starting point

WeSep master (the app's tests used commit `dc11cb1`), recipe
`examples/audio/voxceleb1`, config `confs/tse_bsrnn_spk_online.yaml`, with
the changes below.

## Config changes

```yaml
model:
  tse_model: TSE_BSRNN_SPK
model_args:
  tse_model:
    separator:
      sr: 16000
      win: 512
      stride: 128
      feature_dim: 64      # was 128: see "Size" below
      num_repeat: 4        # was 6
      causal: true         # the change that matters
      nspk: 1
    speaker:
      features:
        listen:  { enabled: false }
        usef:    { enabled: false }   # first version: off (looks at the
        context: { enabled: false }   # enrollment in more costly ways)
        tfmap:   { enabled: false }
        spkemb:
          enabled: true
          input: waveform
          fusion: multiply
      speaker_model:
        speaker_encoder:
          # Must stay this encoder: the app already computes the agent's
          # voice fingerprint with it (speaker.onnx, 192 values).
          model: ECAPA_TDNN_GLOB_c512
          pretrained: ./wespeaker_models/voxceleb_ECAPA512/avg_model.pt
          spk_args: { embed_dim: 192, feat_dim: 80, pooling_func: ASTP }
```

## Data: what the office actually sounds like

The calls that failed in the app were mostly a colleague talking **while the
agent was silent**, at about the agent's own level. Train for that:

```yaml
dataset_args:
  online_mix: true
  chunk_len: 48000
  num_speakers:
    distribution: [0.2, 0.6, 0.2]   # 20% target alone: learn to pass it through
    max_speakers: 4
  timeline:
    two_speaker:
      overlap_ratio: [0.0, 1.0]     # was [0.5, 1.0]: include turn-taking
  mix_snr:
    range: [-5, 10]
  noise_prob: 0.5                   # office noise (MUSAN or your own)
  reverb_prob: 0.3
```

- **Speech:** your IndicVoices data (the languages agents speak), plus
  English. Enrollment clips 10–20 s, like **Register my voice**.
- **Microphone:** if possible, add recordings through Detecta 201 headsets:
  the app's `Recordings\Agent voice\<call>\input.m4a` files are exactly what
  the model will hear (only from calls with the agent's consent).

## Size

Measured on the development PC (Intel i5-12450H, mains power), PyTorch,
untrained weights (speed doesn't depend on training), 10 s of audio:

| feature_dim × num_repeat | Parameters | RTF, 1 thread | RTF, 2 threads |
|---|---|---|---|
| 128 × 6 (WeSep default) | 18.9 M | 0.85 | 0.57 |
| 64 × 6 | 4.9 M | 0.26 | 0.19 |
| **64 × 4 (recommended)** | **4.3 M** | **0.18** | **0.13** |
| 48 × 4 | 2.5 M | 0.11 | 0.08 |

The client's Dell Latitude 3510–3550 (15 W Intel i5 / Core Ultra 5) are
slower, and the live chain also runs DeepFilterNet3 (about 0.3 of a core),
so the TSE step should stay under about RTF 0.3 there: 64 × 4. Use 48 × 4 if
a Latitude 3510 can't keep up.

## Checks before handing it over

1. `check_causal` in `wesep/models/tse_bsrnn_spk.py`: output must not change
   before the input does (the app's test showed 1.976 s for a change at
   2.000 s).
2. SI-SDR on your dev set, against the public model.
3. Turn-taking: a recording where a colleague speaks while the agent is
   silent (like the team's InputMixx) must come out with the colleague well
   down.

## Hand-over

Send the trained `avg_model.pt` and its `config.yaml`. YAL ClearVoice then:

1. exports it to ONNX as a step-by-step (streaming) model, one 8 ms frame at
   a time with its LSTM states carried between frames;
2. checks the streaming output against your PyTorch output;
3. measures it on a Latitude-class CPU;
4. puts it in the live chain's TSE step, before DeepFilterNet3, with the
   agent's registered voice as the enrollment.
