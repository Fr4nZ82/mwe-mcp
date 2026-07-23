#!/usr/bin/env python3
"""mwe daily digest — cron `--script` for the mwe-mcp bridge.

Drains the SYSTEM notices from the reverse channel (everything except
`fact_minted_for_you`, which the `mwe-events` gateway hook delivers
per-recipient in near-real-time) and prints a compact summary: how many
memory changes, of what type, plus the dashboard link. The agent riding
the cron job phrases it for the operator — or stays [SILENT] when the
day was quiet.

Install (once, from the operator's chat or shell):

    hermes cron create "0 9 * * *" \
      "If the script output is NO_EVENTS reply exactly [SILENT]. Otherwise \
compose a short daily memory recap in the operator's language from the \
script output: how many changes and of what type, one line each, then \
offer the dashboard link. Add nothing else. Do not use tools." \
      --script mwe-daily-digest.py \
      --name "mwe daily digest" \
      --deliver telegram

Config: `$HERMES_HOME/mwe.json` (`url`, optional `dashboardUrl` — set it
to the public https origin; the MCP `url` is often loopback) plus
`MWE_TOKEN` in the environment. Stdlib-only. Acks what it summarised:
each notice rides exactly one digest.
"""

from __future__ import annotations

import base64
import importlib.util
import json
import os
import sys
from collections import Counter
from datetime import date
from pathlib import Path

# The system kinds this digest owns. `fact_minted_for_you` is EXCLUDED —
# the gateway hook delivers it per-recipient; the two drains share one
# consumer and stay disjoint by kind filter. A future server kind joins
# the digest by being added here.
KINDS = [
    "structure_applied",
    "auto_applied",
    "dedup_proposed",
    "archive_proposed",
    "document_ingested",
    "compile_failure_streak",
    "recall_tuning_proposed",
]

# Human labels the agent can lift verbatim into the recap.
LABELS = {
    "structure_applied": "pages reorganized (undoable from the dashboard)",
    "auto_applied": "proposals auto-applied (silence confirms them)",
    "dedup_proposed": "duplicate facts merged",
    "archive_proposed": "pages proposed for archival",
    "document_ingested": "documents finished ingesting",
    "compile_failure_streak": "pages repeatedly failing to compile (needs a look)",
    "recall_tuning_proposed": "recall-tuning proposals awaiting the operator",
}

MAX_ROUNDS = 20  # top_k 50 × 20 — far beyond a day's traffic


def hermes_home() -> Path:
    return Path(os.environ.get("HERMES_HOME", str(Path.home() / ".hermes")))


def consumer_id_from_token(token: str) -> str:
    try:
        payload = token.split(".")[1]
        payload += "=" * (-len(payload) % 4)
        claims = json.loads(base64.urlsafe_b64decode(payload))
        return str(claims.get("consumer_id") or "").strip()
    except Exception:
        return ""


def load_client(home: Path):
    path = home / "plugins" / "mwe" / "client.py"
    spec = importlib.util.spec_from_file_location("mwe_digest_client", path)
    if spec is None or spec.loader is None:
        raise ImportError(f"no client at {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.MweClient


def main() -> int:
    home = hermes_home()
    try:
        with open(home / "mwe.json", encoding="utf-8") as f:
            cfg = json.load(f)
    except Exception as e:
        print(f"NO_EVENTS (mwe.json unreadable: {e})")
        return 0
    url = str(cfg.get("url") or "").strip()
    token = os.environ.get("MWE_TOKEN", "").strip()
    if not url or not token:
        print("NO_EVENTS (url/MWE_TOKEN missing)")
        return 0
    consumer_id = consumer_id_from_token(token)
    if not consumer_id:
        print("NO_EVENTS (token has no consumer_id claim)")
        return 0
    dashboard = str(cfg.get("dashboardUrl") or "").strip() or url.rsplit("/mcp", 1)[0]

    client = load_client(home)(url, token)
    counts: Counter = Counter()
    variants: Counter = Counter()
    for _ in range(MAX_ROUNDS):
        outcome = client.call_tool(
            "events_poll",
            {"consumer_id": consumer_id, "kinds": KINDS, "top_k": 50},
        )
        events = outcome.get("events") or []
        if not events:
            break
        for event in events:
            kind = str(event.get("kind") or "?")
            counts[kind] += 1
            if kind == "structure_applied":
                variant = str((event.get("payload") or {}).get("variant") or "").strip()
                if variant:
                    variants[variant] += 1
        client.call_tool(
            "events_ack",
            {
                "consumer_id": consumer_id,
                "event_ids": [e["event_id"] for e in events],
            },
        )
        if not outcome.get("has_more"):
            break

    if not counts:
        print("NO_EVENTS")
        return 0

    print(f"MWE MEMORY DIGEST {date.today().isoformat()}")
    print(f"total changes: {sum(counts.values())}")
    for kind, n in counts.most_common():
        line = f"- {kind}: {n} — {LABELS.get(kind, 'other')}"
        if kind == "structure_applied" and variants:
            detail = ", ".join(f"{v}×{c}" for v, c in variants.most_common())
            line += f" ({detail})"
        print(line)
    print(f"dashboard: {dashboard}/dashboard")
    return 0


if __name__ == "__main__":
    sys.exit(main())
