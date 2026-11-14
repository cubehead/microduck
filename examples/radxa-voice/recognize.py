#!/usr/bin/env python3
"""Read mono PCM16LE from stdin; print final local Mandarin transcriptions."""
import argparse
import json
from pathlib import Path
import sys

PHRASES = {"走", "走路", "站立", "站起来", "坐下", "停止", "停下", "叫一声", "叫一聲"}

def publish(result):
    text = json.loads(result).get("text", "").strip()
    if not text:
        return
    normalized = "".join(text.split())
    print(json.dumps({
        "text": text,
        "known_phrase": normalized if normalized in PHRASES else None,
    }, ensure_ascii=False), flush=True)

def decode(recognizer, source):
    while True:
        frame = source.read(4000)
        if not frame:
            break
        if len(frame) % 2:
            raise ValueError("Odd-length input; expected raw 16-bit PCM.")
        if recognizer.AcceptWaveform(frame):
            publish(recognizer.Result())
    publish(recognizer.FinalResult())

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", type=Path, required=True)
    args = parser.parse_args()
    if not (args.model / "am" / "final.mdl").is_file():
        parser.error("Not an extracted Vosk model directory.")
    if sys.stdin.isatty():
        parser.error("Pipe raw PCM from arecord; do not run without audio input.")
    try:
        from vosk import KaldiRecognizer, Model, SetLogLevel
        SetLogLevel(-1)
        model = Model(str(args.model.resolve()))
        decode(KaldiRecognizer(model, 16000), sys.stdin.buffer)
    except (ImportError, OSError, ValueError) as error:
        parser.exit(1, f"Recognition failed: {error}\n")
    except KeyboardInterrupt:
        return 130
    return 0

if __name__ == "__main__":
    sys.exit(main())
