#!/usr/bin/env python3
"""Live smoke for the hermes bridge — operator-run, against a REAL server.

Drives the provider (loaded through hermes's own discovery) through a short
scripted conversation against a running `mwe-mcp serve` on a throwaway
workdir: per-turn ingest, window threading, an explicit search, the
dashboard link. Costs internal-LLM calls. Prints the recall blocks for the
operator to eyeball; asserts only mechanics (no exception, parseable
responses) — memory *quality* is judged by the human reading the output.

Env (all required):
    HERMES_SRC        local hermes-agent checkout (for the plugin seams)
    MWE_MCP_URL       e.g. http://127.0.0.1:8742/mcp
    MWE_TOKEN         consumer bearer token (the bridge's bot identity)
    MWE_PRIMARY_USER  mwe user id the conversation acts as
Optional:
    MWE_LOCALE        BCP-47 tag forwarded per turn (default it-IT)
"""

import json
import os
import sys
import tempfile
from pathlib import Path

TURNS = [
    "ciao! oggi ho piantato i pomodori nella serra nuova",
    "ricordami una cosa: giovedì passa l'idraulico alle 9",
    "cosa ti ho detto della serra?",
]


def main():
    src = os.environ.get("HERMES_SRC", "")
    url = os.environ.get("MWE_MCP_URL", "")
    token = os.environ.get("MWE_TOKEN", "")
    user = os.environ.get("MWE_PRIMARY_USER", "")
    if not (src and url and token and user):
        print(__doc__)
        return 2
    sys.path.insert(0, src)

    home = Path(tempfile.mkdtemp(prefix="mwe-hermes-live."))
    (home / "plugins").mkdir()
    (home / "plugins" / "mwe").symlink_to(
        Path(__file__).resolve().parent / "plugins" / "memory" / "mwe")
    (home / "mwe.json").write_text(json.dumps({
        "url": url, "primaryUser": user,
        "locale": os.environ.get("MWE_LOCALE", "it-IT"),
        "maxWindow": 16,
    }))
    os.environ["HERMES_HOME"] = str(home)

    from plugins.memory import load_memory_provider
    provider = load_memory_provider("mwe")
    assert provider is not None, "provider failed to load"
    provider.initialize(session_id="live-smoke", hermes_home=str(home),
                        platform="cli", agent_context="primary")
    assert provider.is_available(), "provider not available — check url/token"

    print(f"=== live smoke: {url} acting as {user} ===\n")
    for i, text in enumerate(TURNS, 1):
        print(f"--- turn {i} · user: {text}")
        block = provider.prefetch(text)
        print(f"recall block:\n{block or '(empty)'}\n")
        provider.sync_turn(text, f"(scripted assistant reply {i})")

    print("--- explicit search: 'serra'")
    print(provider.handle_tool_call("mwe_search", {"query": "serra", "top_k": 5}), "\n")
    print("--- dashboard link")
    print(provider.handle_tool_call("mwe_dashboard_link", {}), "\n")

    provider.shutdown()
    print("live smoke completed — read the blocks above to judge recall quality")
    return 0


if __name__ == "__main__":
    sys.exit(main())
