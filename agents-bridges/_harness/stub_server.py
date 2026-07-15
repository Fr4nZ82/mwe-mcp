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
    "dashboard_link": {"dashboard_path": "/dashboard/"},
}


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
                    with stub._lock:
                        stub.requests.append({
                            "tool": tool,
                            "arguments": params.get("arguments", {}),
                            "headers": {k.lower(): v for k, v in self.headers.items()},
                        })
                        payload = stub.responses.get(tool)
                    if payload is None:
                        return self._result(rid, {
                            "content": [{"type": "text", "text": f"unknown tool: {tool}"}],
                            "isError": True,
                        })
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
