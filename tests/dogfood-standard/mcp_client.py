#!/usr/bin/env python3
"""Minimal MCP standard-consumer driver for the mwe-mcp dogfood.

Drives the server's Streamable-HTTP MCP endpoint directly (stateless,
no Mcp-Session-Id) so we can exercise the *consumer* tool surface
without adopting the consumer contract (no hooks, no skills) — this is
"test the server without becoming a consumer". See instruction.md.

Usage:
    MWE_JWT_FILE=/path/to/<user>.jwt python3 mcp_client.py <tool> '<json-args>'

Env:
    MWE_JWT_FILE  path to a file holding the Bearer JWT (default /tmp/mwe-test.jwt)
    MWE_MCP_URL   MCP endpoint (default http://127.0.0.1:8742/mcp)

Examples:
    MWE_JWT_FILE=tokens/frodo.jwt python3 mcp_client.py wiki_search '{"query":"cane"}'
    MWE_JWT_FILE=tokens/miriam.jwt python3 mcp_client.py wiki_read '{"wiki_id":"frodo"}'
    MWE_JWT_FILE=tokens/frodo.jwt python3 mcp_client.py wiki_ingest_message \
        '{"text":"manca il detersivo","context_hint":"conversation"}'
"""
import sys
import os
import json
import urllib.request
import urllib.error

JWT = open(os.environ.get("MWE_JWT_FILE", "/tmp/mwe-test.jwt")).read().strip()
URL = os.environ.get("MWE_MCP_URL", "http://127.0.0.1:8742/mcp")
# Diagonal identity model: a *standard* consumer is a credential-less system
# user (the bot). It speaks for a human by setting X-MWE-Act-As to that human's
# user_id; the server resolves it against the token's consumer_delegations.
# Set MWE_ACT_AS=<human_id> to attribute this call to that sender.
ACT_AS = os.environ.get("MWE_ACT_AS", "").strip()


def rpc(method, params, rid=1):
    body = json.dumps(
        {"jsonrpc": "2.0", "id": rid, "method": method, "params": params}
    ).encode()
    headers = {
        "Authorization": "Bearer " + JWT,
        "Content-Type": "application/json",
        # The server speaks Streamable HTTP — it may answer with a
        # single JSON body or an SSE stream; advertise both.
        "Accept": "application/json, text/event-stream",
    }
    if ACT_AS:
        headers["X-MWE-Act-As"] = ACT_AS
    req = urllib.request.Request(URL, data=body, method="POST", headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=180) as r:
            return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e:
        raw = e.read().decode(errors="replace")
        try:
            return e.code, json.loads(raw)
        except Exception:
            return e.code, {"_raw": raw}


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(2)
    tool = sys.argv[1]
    args = json.loads(sys.argv[2]) if len(sys.argv) > 2 else {}
    status, resp = rpc("tools/call", {"name": tool, "arguments": args}, rid=99)
    print("HTTP", status)
    if "error" in resp:
        print("JSONRPC ERROR:", json.dumps(resp["error"], indent=2, ensure_ascii=False))
        return
    res = resp.get("result", resp)
    if isinstance(res, dict) and res.get("isError"):
        print("** tool isError = true **")
    printed = False
    for c in res.get("content", []) if isinstance(res, dict) else []:
        if c.get("type") == "text":
            printed = True
            t = c["text"]
            try:
                print(json.dumps(json.loads(t), indent=2, ensure_ascii=False))
            except Exception:
                print(t)
    if isinstance(res, dict) and res.get("structuredContent"):
        print("--- structuredContent ---")
        print(json.dumps(res["structuredContent"], indent=2, ensure_ascii=False))
        printed = True
    if not printed:
        print("--- raw result ---")
        print(json.dumps(resp, indent=2, ensure_ascii=False)[:2500])


if __name__ == "__main__":
    main()
