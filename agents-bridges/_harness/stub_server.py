#!/usr/bin/env python3
"""In-process stub of the mwe-mcp MCP endpoint, for offline bridge smokes.

Serves just enough Streamable-HTTP JSON-RPC for a bridge to complete its
per-turn loop with no real server, no engine, no LLM: `initialize`,
`tools/list`, and `tools/call` with canned per-tool payloads. Every
`tools/call` is recorded (tool name, arguments, auth/act-as headers) so a
smoke can assert the contract mechanics — exactly one ingest per turn, the
window threaded and trimmed, the act-as header per sender.

    from stub_server import StubMwe

    with StubMwe() as stub:
        bridge_under_test.configure(url=stub.url, token="test-jwt")
        bridge_under_test.run_turn("hello")
        ingests = stub.calls("wiki_ingest_message")
        assert len(ingests) == 1
        assert ingests[0]["headers"].get("x-mwe-act-as") == "anna"

Canned payloads can be overridden per tool, and swapped mid-test to
script a multi-turn flow (e.g. a `needs_disambig` first turn):

    stub.responses["wiki_ingest_message"]["needs_disambig"] = True

An ingest that names a `disambig_choice` is the commit, and the stub
answers it the way the server does — settled, with no candidates — no
matter what the scripted payload says.

The two governance blocks of an ingest response ride the same knob. They
are **absent** from the default payload, because the server sends each
one only on the turn it applies to; a smoke that wants one splices the
canonical fixture in:

    stub.responses["wiki_ingest_message"]["pending_votes"] = PENDING_VOTES

The stub also serves the out-of-band media endpoint (`POST …/media`,
multipart/form-data — the media-pipeline design note): each upload
is recorded in `stub.media_uploads` ({kind, caption, description,
filename, size, headers}) and answered with a canned catalog response
whose NNN counter increments per upload.

Standard library only.
"""

import copy
import email.parser
import email.policy
import json
import os
import threading
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# Mirrors the wire payload of `call_wiki_ingest_message`
# (crates/mwe-mcp-server/src/mcp/tools.rs).
DEFAULT_RESPONSES = {
    "wiki_ingest_message": {
        "intent_classified": "recall",
        "context_snippet": "Recall: (stub) nothing relevant on file.",
        "suggested_seed": "(stub seed — rewrite, never forward verbatim)",
        "recent_window": "RECENT EXCHANGES ON YOUR OTHER CHANNELS WITH THIS USER "
                         "(stub): [just now · via bot/salotto] user: ciao dal salotto",
        "capture_id": None,
        "needs_disambig": False,
        "disambig_candidates": [],
        "llm_used": "stub",
        "took_ms": 1,
    },
    "wiki_search": {"hits": []},
    "events_poll": {"events": []},
    "events_ack": {"acked": 0},
    # `call_dashboard_link` mints the link as a **path**: the server does not
    # know the origin it is reached at, so putting an origin in front of it is
    # the bridge's job and the fixture is the shape that makes that visible.
    "dashboard_link": {
        "url": "/dashboard/auth/link?token=stub-jwt&next=%2Fdashboard%2Fhome",
        "token_expires_at": "2026-06-12T10:10:00Z",
        "base_ttl_seconds": 600,
    },
}

# The `pending_votes` block of an ingest response: the acting member owes a
# vote on a request to forget a fact they are part of. Voting is a dashboard
# action, so the block carries where to go, not a way to vote from here — and
# no fact text, only its id.
#
# The deadline is relative because a bridge reads it against the clock to
# decide whether to raise the vote at all: a fixed date would drift out of the
# window it is meant to sit inside and quietly test the other branch.
PENDING_VOTES_DEADLINE = (
    datetime.now(timezone.utc) + timedelta(hours=6)
).isoformat().replace("+00:00", "Z")

PENDING_VOTES = {
    "count": 1,
    "requests": [{
        "proposal_id": "p-forget-1",
        "fact_id": "f-2026-06-12-0001",
        "requester": "bob",
        "deadline": PENDING_VOTES_DEADLINE,
        "dashboard_path": "/dashboard/proposals/p-forget-1/open-in-chat",
    }],
    "dashboard_path": "/dashboard/proposals",
    "note": "vote_no_to_block_silence_is_consent",
}

# The `document_promoted` block: the turn was document-shaped, so the server
# archived it verbatim on the media rail and queued it for document ingestion
# instead of filing the whole paste as one message.
DOCUMENT_PROMOTED = {
    "catalog_id": "c-2026-06-12-doc-001.txt",
    "job_id": "j-2026-06-12-0007",
    "existing": False,
}


def _commit_settles_it(tool, arguments, payload):
    """The server's second-turn rule, in the fixture that stands in for it.

    An ingest naming a `disambig_choice` commits: the orchestrator answers
    `needs_disambig: false` with no candidates, whatever the classifier said
    (`crates/mwe-core/src/ingest.rs`, `resolving_disambig`). A stub that kept
    asking would let a bridge ship a loop no real server can produce.
    """
    if tool != "wiki_ingest_message" or not isinstance(payload, dict):
        return payload
    metadata = arguments.get("metadata")
    if not isinstance(metadata, dict) or not metadata.get("disambig_choice"):
        return payload
    return {**payload, "needs_disambig": False, "disambig_candidates": []}


class StubMwe:
    """A throwaway MCP endpoint on 127.0.0.1:<ephemeral>."""

    def __init__(self, responses=None):
        self.responses = copy.deepcopy(DEFAULT_RESPONSES)
        if responses:
            self.responses.update(copy.deepcopy(responses))
        self.requests = []  # every tools/call: {tool, arguments, headers}
        self._media_uploads = []  # every POST /media: {kind, caption, …}
        self._lock = threading.Lock()
        stub = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length", "0"))
                raw = self.rfile.read(length)
                # The out-of-band media endpoint lives at the MCP origin
                # minus the trailing /mcp, plus /media.
                if self.path.split("?", 1)[0].rstrip("/").endswith("/media"):
                    return self._media_upload(raw)
                try:
                    msg = json.loads(raw)
                except ValueError:
                    return self._reply(400, {"error": "bad json"})
                method = msg.get("method", "")
                rid = msg.get("id")
                if method == "initialize":
                    return self._result(rid, {
                        "protocolVersion": "2025-03-26",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "mwe-stub", "version": "0"},
                    })
                if method.startswith("notifications/"):
                    self.send_response(202)
                    self.end_headers()
                    return None
                if method == "tools/list":
                    return self._result(rid, {
                        "tools": [
                            {"name": n, "description": "stub", "inputSchema": {"type": "object"}}
                            for n in stub.responses
                        ]
                    })
                if method == "tools/call":
                    params = msg.get("params", {})
                    tool = params.get("name", "")
                    arguments = params.get("arguments", {}) or {}
                    with stub._lock:
                        stub.requests.append({
                            "tool": tool,
                            "arguments": arguments,
                            "headers": {k.lower(): v for k, v in self.headers.items()},
                        })
                        payload = stub.responses.get(tool)
                    if payload is None:
                        return self._result(rid, {
                            "content": [{"type": "text", "text": f"unknown tool: {tool}"}],
                            "isError": True,
                        })
                    payload = _commit_settles_it(tool, arguments, payload)
                    return self._result(rid, {
                        "content": [{"type": "text", "text": json.dumps(payload)}],
                        "isError": False,
                    })
                return self._reply(200, {
                    "jsonrpc": "2.0", "id": rid,
                    "error": {"code": -32601, "message": f"unknown method: {method}"},
                })

            def _media_upload(self, raw):
                """Parse a multipart upload; record it; mint a canned id."""
                ctype = self.headers.get("Content-Type", "")
                if "multipart/form-data" not in ctype:
                    return self._reply(400, {"error": {
                        "code": "malformed_multipart",
                        "message": "expected multipart/form-data",
                    }})
                try:
                    parsed = email.parser.BytesParser(
                        policy=email.policy.default
                    ).parsebytes(
                        b"Content-Type: " + ctype.encode() + b"\r\n\r\n" + raw
                    )
                    fields, file_part = {}, None
                    for part in parsed.iter_parts():
                        name = part.get_param("name", header="content-disposition")
                        if name == "file":
                            file_part = part
                        elif name:
                            fields[name] = part.get_content().strip()
                except Exception as e:
                    return self._reply(400, {"error": {
                        "code": "malformed_multipart", "message": str(e),
                    }})
                if file_part is None:
                    return self._reply(400, {"error": {
                        "code": "missing_file_field",
                        "message": "multipart body must carry a `file` field",
                    }})
                kind = fields.get("kind", "")
                if not kind:
                    return self._reply(400, {"error": {
                        "code": "missing_kind_field",
                        "message": "multipart body must carry a `kind` field",
                    }})
                filename = file_part.get_filename()
                payload = file_part.get_payload(decode=True) or b""
                ext = os.path.splitext(filename or "")[1] or ".jpg"
                with stub._lock:
                    n = len(stub._media_uploads) + 1
                    stub._media_uploads.append({
                        "kind": kind,
                        "caption": fields.get("caption"),
                        "description": fields.get("description"),
                        "filename": filename,
                        "size": len(payload),
                        "headers": {k.lower(): v for k, v in self.headers.items()},
                    })
                # 201 = fresh store (the dedup=true answer is 200; the
                # stub never dedups).
                return self._reply(201, {
                    "catalog_id": f"c-2026-06-12-{kind}-{n:03d}{ext}",
                    "kind": kind,
                    "mime": file_part.get_content_type(),
                    "sha256": "0" * 64,
                    "size_bytes": len(payload),
                    "dedup": False,
                })

            def _result(self, rid, result):
                self._reply(200, {"jsonrpc": "2.0", "id": rid, "result": result})

            def _reply(self, status, body):
                data = json.dumps(body).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        self._server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)

    @property
    def url(self):
        host, port = self._server.server_address
        return f"http://{host}:{port}/mcp"

    def calls(self, tool=None):
        with self._lock:
            if tool is None:
                return list(self.requests)
            return [r for r in self.requests if r["tool"] == tool]

    @property
    def media_uploads(self):
        with self._lock:
            return list(self._media_uploads)

    def start(self):
        self._thread.start()
        return self

    def stop(self):
        self._server.shutdown()
        self._server.server_close()

    def __enter__(self):
        return self.start()

    def __exit__(self, *exc):
        self.stop()
