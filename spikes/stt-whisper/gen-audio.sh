#!/usr/bin/env bash
# Regenerate the (gitignored) test WAVs from the committed scripts, using macOS TTS.
#
# SPIKE-GRADE PROXY (deviation from plan): the plan calls for dam recording himself reading
# the jargon scripts (quiet + jobsite-noise). The spike worker cannot record a human, so we
# synthesize the read-aloud with macOS `say` (TTS). This is CLEANER than human+jobsite audio,
# so the resulting WER is OPTIMISTIC — good for relative model/biasing comparison, not an
# absolute accuracy claim. The "noisy" condition is added in-code via `--snr` (synthetic
# additive white noise) rather than real ambience — see wer.rs. Real jobsite audio remains
# the aspirational corpus for Plan 06.
#
# Because TTS reads the script verbatim, the committed audio/references/*.txt ARE the exact
# ground-truth transcripts.
set -euo pipefail
cd "$(dirname "$0")/audio"

for n in jargon1 jargon2; do
  say -f "scripts/$n.txt" -o "/tmp/$n.aiff"
  # -d LEI16@16000 forces a true resample to 16 kHz mono (whisper's required input format).
  afconvert "/tmp/$n.aiff" -o "$n.wav" -f WAVE -d LEI16@16000 -c 1
  afinfo "$n.wav" 2>/dev/null | grep -iE "Data format|duration"
done
