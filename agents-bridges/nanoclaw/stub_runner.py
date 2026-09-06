#!/usr/bin/env python3
"""Run the shared MCP stub as a subprocess, for a smoke written in another language.

`_harness/stub_server.py` is an in-process recorder: a Python smoke holds the
`StubMwe` object and reads `stub.calls()` directly. The nanoclaw bridge's smoke
is TypeScript — it drives nanoclaw's own poll loop under bun — so the stub runs
here instead and the two sides meet through two files:

- **`--calls`** — every `tools/call` and every media upload, rewritten a few
  times a second. The smoke polls it and asserts on it.
- **`--responses`** — canned per-tool payloads, re-read before each answer. The
  smoke writes it to script a turn: a `needs_disambig` first turn, or the fail
  marker below, which makes the stub answer an error and proves the turn
  survives without memory.

    stub_runner.py --url-file url.txt --calls calls.json --responses responses.json

The URL lands in `--url-file` once the socket is up. SIGTERM stops it.

Standard library only, and not a bridge of its own: it adds no behaviour the
stub does not already have.
"""

import argparse
import json
import os
import signal
import sys
import time
from pathlib import Path

HARNESS = Path(__file__).resolve().parent.parent / "_harness"
sys.path.insert(0, str(HARNESS))

from stub_server import StubMwe  # noqa: E402

# How often the two files are synced. Fast enough that a smoke asserting right
# after a turn sees that turn, cheap enough to leave running.
SYNC_INTERVAL = 0.05

# The tool name a smoke uses to make the stub answer with an HTTP error instead
# of a payload — the degradation path, where the turn must survive.
FAIL_MARKER = "__fail__"


def write_atomic(path: Path, payload) -> None:
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(json.dumps(payload, ensure_ascii=False))
    os.replace(tmp, path)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url-file", required=True)
    parser.add_argument("--calls", required=True)
    parser.add_argument("--responses", required=True)
    args = parser.parse_args()

    calls_path = Path(args.calls)
    responses_path = Path(args.responses)

    with StubMwe() as stub:
        Path(args.url_file).write_text(stub.url)
        write_atomic(calls_path, {"calls": [], "media": []})

        last_responses = ""
        while True:
            # A response file the smoke rewrote mid-run scripts the next turn:
            # entries replace the canned payload for that tool, and a tool
            # mapped to the fail marker is removed so the stub answers isError.
            try:
                raw = responses_path.read_text()
            except OSError:
                raw = ""
            if raw and raw != last_responses:
                last_responses = raw
                try:
                    scripted = json.loads(raw)
                except ValueError:
                    scripted = {}
                for tool, payload in scripted.items():
                    if payload == FAIL_MARKER:
                        stub.responses.pop(tool, None)
                    else:
                        stub.responses[tool] = payload

            write_atomic(calls_path, {"calls": stub.calls(), "media": stub.media_uploads})
            time.sleep(SYNC_INTERVAL)
    return 0


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    sys.exit(main())
