#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10,<3.14"
# dependencies = [
#     "essentia",
# ]
# ///
# scripts/analyze.py
#
# Audio analysis script for Performative.
#
# Accepts a single file path argument, analyzes the audio with Essentia, and
# writes a JSON object to stdout matching the TrackAnalysis Rust struct:
#
#   {
#     "bpm": <float>,
#     "key": "<note> <scale>",
#     "beats": [<float>, ...],
#     "downbeats": [<float>, ...],
#     "duration_secs": <float>
#   }
#
# Downbeats are estimated as every 4th beat starting from beat index 0.
#
# Exit codes:
#   0  success — JSON written to stdout
#   1  bad arguments or file not found
#   2  Essentia not installed
#   3  analysis error

import json
import os
import sys


def main() -> int:
    """
    purpose: parse arguments, run analysis, and write JSON to stdout.
    @return: exit code (0 = success, non-zero = error)
    """
    if len(sys.argv) != 2:
        print(
            f"usage: {os.path.basename(sys.argv[0])} <audio_file>",
            file=sys.stderr,
        )
        return 1

    audio_path = sys.argv[1]

    if not os.path.isfile(audio_path):
        print(f"error: file not found: {audio_path}", file=sys.stderr)
        return 1

    try:
        import essentia.standard as es
    except ImportError:
        print(
            "error: essentia is not installed. Run this script via: uv run scripts/analyze.py <file>",
            file=sys.stderr,
        )
        return 2

    try:
        result = analyze(audio_path, es)
    except Exception as exc:  # noqa: BLE001
        print(f"error: analysis failed: {exc}", file=sys.stderr)
        return 3

    json.dump(result, sys.stdout, separators=(",", ":"))
    sys.stdout.write("\n")
    return 0


def analyze(audio_path: str, es) -> dict:
    """
    purpose: load audio and run BPM, beat, and key extraction via Essentia.

    Downbeats are estimated as every 4th beat starting from beat index 0, which
    is a reasonable approximation when true downbeat detection is not available.

    @param audio_path: absolute or relative path to the audio file
    @param es: the essentia.standard module (injected to allow testing)
    @return: dict with keys bpm, key, beats, downbeats, duration_secs
    """
    # Load audio as mono, at the native sample rate.
    loader = es.MonoLoader(filename=audio_path)
    audio = loader()

    sample_rate = 44100.0  # MonoLoader defaults to 44100 Hz
    duration_secs = len(audio) / sample_rate

    # BPM and beat tracking.
    rhythm_extractor = es.RhythmExtractor2013(method="multifeature")
    bpm, beats, beats_confidence, _, beats_intervals = rhythm_extractor(audio)

    beats_list = [float(b) for b in beats]

    # Downbeats: every 4th beat starting from index 0.
    downbeats_list = [beats_list[i] for i in range(0, len(beats_list), 4)]

    # Key detection.
    key_extractor = es.KeyExtractor()
    key, scale, key_strength = key_extractor(audio)
    key_str = f"{key} {scale}"

    return {
        "bpm": float(bpm),
        "key": key_str,
        "beats": beats_list,
        "downbeats": downbeats_list,
        "duration_secs": float(duration_secs),
    }


if __name__ == "__main__":
    sys.exit(main())
