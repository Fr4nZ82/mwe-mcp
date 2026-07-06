#!/usr/bin/env python3
"""Reference MCP client for mwe-mcp bridges.

Speaks the server's Streamable-HTTP MCP endpoint directly (stateless,
no Mcp-Session-Id): POST one JSON-RPC `tools/call` per invocation with
a Bearer JWT and, optionally, the `X-MWE-Act-As` header that attributes
the call to a delegated human sender. Standard library only — usable
verbatim inside any Python bridge or smoke script, and the model for a
bridge's own client in other languages.

Importable:

    from mwe_client import MweClient
    client = MweClient(url, jwt, act_as="anna")
    resp = client.call_tool("wiki_ingest_message", {
        "text": "manca il detersivo",
        "recent_messages": [{"role": "user", "text": "ciao"}],
        "metadata": {"locale": "it-IT"},
    })
    print(resp["context_snippet"])

CLI (mirrors tests/dogfood-standard/mcp_client.py):

    MWE_JWT_FILE=tokens/bot.jwt python3 mwe_client.py wiki_search '{"query":"cane"}'

Env (CLI only):
    MWE_JWT        the Bearer JWT itself, or
    MWE_JWT_FILE   path to a file holding it (default /tmp/mwe-test.jwt)
    MWE_MCP_URL    MCP endpoint (default http://127.0.0.1:8742/mcp)
    MWE_ACT_AS     delegated human sender id for X-MWE-Act-As
"""

import json
import mimetypes
import os
import sys
import urllib.error
import urllib.request
import uuid


class MweError(RuntimeError):
    """A JSON-RPC error or a tool-level `isError` result."""

    def __init__(self, message, *, code=None, data=None):
        super().__init__(message)
        self.code = code
        self.data = data


class MweClient:
    """One logical MCP connection: endpoint + token + fixed act-as.

    A bridge serving several delegated humans keeps one instance per
    sender (the per-sender pool): the act-as identity is fixed at
    construction, mirroring clients that can only set headers at
    connect time.
    """

    def __init__(self, url, jwt, *, act_as="", timeout=180):
        self.url = url
        self.jwt = jwt.strip()
        self.act_as = act_as.strip()
        self.timeout = timeout

    def raw_rpc(self, method, params, *, rid=1):
        """POST one JSON-RPC request; return (http_status, parsed_body)."""
        body = json.dumps(
            {"jsonrpc": "2.0", "id": rid, "method": method, "params": params}
        ).encode()
        headers = {
            "Authorization": "Bearer " + self.jwt,
            "Content-Type": "application/json",
            # Streamable HTTP: the server may answer with a plain JSON
            # body or an SSE stream; advertise both.
            "Accept": "application/json, text/event-stream",
        }
        if self.act_as:
            headers["X-MWE-Act-As"] = self.act_as
        req = urllib.request.Request(self.url, data=body, method="POST", headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as r:
                return r.status, json.loads(r.read())
        except urllib.error.HTTPError as e:
            raw = e.read().decode(errors="replace")
            try:
                return e.code, json.loads(raw)
            except ValueError:
                return e.code, {"_raw": raw}

    def call_tool(self, tool, arguments=None):
        """Call one MCP tool and return its payload as a dict.

        Prefers the JSON text content (the server serialises the tool
        payload there); falls back to `structuredContent`. Raises
        `MweError` on transport, JSON-RPC, or tool-level errors.
        """
        status, resp = self.raw_rpc(
            "tools/call", {"name": tool, "arguments": arguments or {}}, rid=99
        )
        if "error" in resp:
            err = resp["error"]
            raise MweError(
                f"{tool}: JSON-RPC error {err.get('code')}: {err.get('message')}",
                code=err.get("code"),
                data=err.get("data"),
            )
        if status >= 400:
            raise MweError(f"{tool}: HTTP {status}: {resp}")
        result = resp.get("result", {})
        payload = None
        for c in result.get("content", []):
            if c.get("type") == "text":
                try:
                    payload = json.loads(c["text"])
                except ValueError:
                    payload = {"text": c["text"]}
                break
        if payload is None:
            payload = result.get("structuredContent")
        if result.get("isError"):
            raise MweError(f"{tool}: tool error: {payload}", data=payload)
        if payload is None:
            raise MweError(f"{tool}: no content in result: {result}")
        return payload

    def upload_media(self, path, kind, caption=None, description=None, mime=None):
        """Upload one media file to the server's out-of-band byte endpoint.

        The MCP ingest stays JSON: bytes travel through `POST <origin>/media`
        (multipart/form-data), then ride `wiki_ingest_message.attachments`
        as catalog ids — see the server's media pipeline design
        (the media-pipeline design note). The origin is the MCP url
        minus its trailing `/mcp`; the bearer + act-as headers are the same
        as the MCP calls, so the upload is attributed to the same effective
        principal.

        `kind` is the catalog enum: `photo` | `video` | `audio` | `doc`.
        `mime` overrides the extension-based guess (hosts often know the
        authoritative content type). Returns the parsed catalog response
        (`{"catalog_id", "kind", "mime", "sha256", "size_bytes", "dedup"}`);
        raises `MweError` on HTTP or wire errors.
        """
        media_url = self.url.rsplit("/mcp", 1)[0] + "/media"
        filename = os.path.basename(path) or "blob"
        # Multipart headers are line-oriented: a quote or newline in the
        # filename would corrupt the part header.
        filename = filename.replace('"', "_").replace("\r", "_").replace("\n", "_")
        content_type = (
            mime or mimetypes.guess_type(filename)[0] or "application/octet-stream"
        )
        with open(path, "rb") as f:
            payload = f.read()

        boundary = "mwe-multipart-" + uuid.uuid4().hex
        body = bytearray()

        def text_field(name, value):
            body.extend(
                (
                    f"--{boundary}\r\n"
                    f'Content-Disposition: form-data; name="{name}"\r\n\r\n'
                    f"{value}\r\n"
                ).encode()
            )

        text_field("kind", kind)
        if caption:
            text_field("caption", caption)
        if description:
            text_field("description", description)
        body.extend(
            (
                f"--{boundary}\r\n"
                f'Content-Disposition: form-data; name="file"; filename="{filename}"\r\n'
                f"Content-Type: {content_type}\r\n\r\n"
            ).encode()
        )
        body.extend(payload)
        body.extend(f"\r\n--{boundary}--\r\n".encode())

        headers = {
            "Authorization": "Bearer " + self.jwt,
            "Content-Type": f"multipart/form-data; boundary={boundary}",
            "Accept": "application/json",
        }
        if self.act_as:
            headers["X-MWE-Act-As"] = self.act_as
        req = urllib.request.Request(
            media_url, data=bytes(body), method="POST", headers=headers
        )
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as r:
                resp = json.loads(r.read())
        except urllib.error.HTTPError as e:
            raw = e.read().decode(errors="replace")
            try:
                parsed = json.loads(raw)
            except ValueError:
                raise MweError(f"media upload: HTTP {e.code}: {raw[:200]}") from None
            err = parsed.get("error") or {}
            raise MweError(
                f"media upload: HTTP {e.code}: "
                f"{err.get('code')}: {err.get('message')}",
                code=err.get("code"),
                data=parsed,
            ) from None
        if not isinstance(resp, dict) or "catalog_id" not in resp:
            raise MweError(f"media upload: unexpected response: {resp}")
        return resp


def _main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    jwt = os.environ.get("MWE_JWT", "")
    if not jwt:
        with open(os.environ.get("MWE_JWT_FILE", "/tmp/mwe-test.jwt")) as f:
            jwt = f.read()
    client = MweClient(
        os.environ.get("MWE_MCP_URL", "http://127.0.0.1:8742/mcp"),
        jwt,
        act_as=os.environ.get("MWE_ACT_AS", ""),
    )
    tool = sys.argv[1]
    args = json.loads(sys.argv[2]) if len(sys.argv) > 2 else {}
    try:
        payload = client.call_tool(tool, args)
    except MweError as e:
        print("ERROR:", e)
        return 1
    print(json.dumps(payload, indent=2, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(_main())
