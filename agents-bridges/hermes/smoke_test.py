#!/usr/bin/env python3
"""Offline smoke for the hermes bridge — run via smoke.sh.

Asserts the per-turn contract (v1) mechanics through hermes-agent's REAL
plugin seams (`plugins.memory.load_memory_provider`,
`plugins.context_engine.load_context_engine`, and — for the media half —
`hermes_cli.plugins.PluginManager` + `invoke_hook("pre_gateway_dispatch")`)
against the recording stub endpoint. No mwe-mcp server, no LLM. Expects
HERMES_HOME pointing at a scratch home with the provider and the
`mwe-media` hook plugin symlinked in, and PYTHONPATH pointing at a
hermes-agent checkout with the engine symlinked in (smoke.sh sets both).
"""

import json
import os
import sys
from pathlib import Path

BRIDGE = Path(__file__).resolve().parent
sys.path.insert(0, str(BRIDGE.parent / "_harness"))

from stub_server import StubMwe  # noqa: E402

HOME = Path(os.environ["HERMES_HOME"])
PASSED = 0


def ok(label, cond, detail=""):
    global PASSED
    if not cond:
        print(f"FAIL: {label} {detail}")
        sys.exit(1)
    PASSED += 1
    print(f"ok   {label}")


def fresh_provider(url, **init_kwargs):
    """Load the provider through hermes's real discovery and initialize it."""
    from plugins.memory import load_memory_provider
    provider = load_memory_provider("mwe")
    assert provider is not None, "provider not discovered in $HERMES_HOME/plugins"
    kwargs = dict(hermes_home=str(HOME), platform="cli", agent_context="primary")
    kwargs.update(init_kwargs)
    provider.initialize(session_id="smoke", **kwargs)
    return provider


def main():
    (HOME / "mwe.json").write_text(json.dumps({
        "url": "http://127.0.0.1:1/mcp",  # repointed per StubMwe below
        "primaryUser": "anna",
        "senderMap": {"telegram:42": "bruno"},
        "locale": "it-IT",
        "maxWindow": 4,
    }))
    # The media hook plugin is opt-in: hermes's general plugin manager
    # loads a user-installed standalone plugin only when its name is in
    # config.yaml's `plugins.enabled` allow-list.
    (HOME / "config.yaml").write_text("plugins:\n  enabled:\n    - mwe-media\n")
    os.environ["MWE_TOKEN"] = "test-jwt"

    # --- discovery through the real seams --------------------------------
    from plugins.memory import discover_memory_providers
    names = [n for n, _, _ in discover_memory_providers()]
    ok("provider discovered out-of-tree", "mwe" in names, f"got {names}")

    from plugins.context_engine import load_context_engine
    engine = load_context_engine("mwe-truncate")
    ok("engine discovered in-tree", engine is not None and engine.name == "mwe-truncate")

    # hermes builds its tool→provider dispatch map at REGISTRATION time,
    # before initialize(): the schemas must be visible on a fresh provider.
    from plugins.memory import load_memory_provider
    uninit = load_memory_provider("mwe")
    ok("tool schemas visible before initialize",
       {s["name"] for s in uninit.get_tool_schemas()}
       == {"mwe_search", "mwe_dashboard_link", "mwe_disambig_commit"})
    out = json.loads(uninit.handle_tool_call("mwe_search", {"query": "x"}))
    ok("uninitialized tool call degrades to error", "error" in out)

    with StubMwe() as stub:
        def stub_mwe_json(**extra):
            cfg = {"url": stub.url, "primaryUser": "anna",
                   "senderMap": {"telegram:42": "bruno"},
                   "locale": "it-IT", "maxWindow": 4}
            cfg.update(extra)
            (HOME / "mwe.json").write_text(json.dumps(cfg))

        stub_mwe_json()
        provider = fresh_provider(stub.url)
        ok("is_available with config+token", provider.is_available())
        ok("static system prompt block", "mwe" in provider.system_prompt_block())

        # --- turn 1: one mechanical ingest, empty window -------------------
        block = provider.prefetch("ciao, sono tornato")
        calls = stub.calls("wiki_ingest_message")
        ok("one ingest per turn", len(calls) == 1, f"got {len(calls)}")
        ok("recall block returned", "stub" in block and "Reply material" in block)
        ok("act-as = primary human", calls[0]["headers"].get("x-mwe-act-as") == "anna")
        ok("bearer token sent", calls[0]["headers"].get("authorization") == "Bearer test-jwt")
        ok("locale forwarded", calls[0]["arguments"]["metadata"]["locale"] == "it-IT")
        ok("turn 1 has no window", "recent_messages" not in calls[0]["arguments"])

        # --- turn 2: consumer-owned window threaded ------------------------
        provider.sync_turn("ciao, sono tornato", "bentornato!")
        provider.prefetch("cosa mi ero segnato per la serra?")
        calls = stub.calls("wiki_ingest_message")
        ok("still one ingest per turn", len(calls) == 2, f"got {len(calls)}")
        window = calls[1]["arguments"]["recent_messages"]
        ok("window order + roles",
           [m["role"] for m in window] == ["user", "assistant"]
           and window[0]["text"] == "ciao, sono tornato"
           and window[1]["text"] == "bentornato!")

        # --- window trim at maxWindow --------------------------------------
        for i in range(5):
            provider.sync_turn(f"u{i}", f"a{i}")
        provider.prefetch("trim check")
        window = stub.calls("wiki_ingest_message")[-1]["arguments"]["recent_messages"]
        ok("window trimmed to maxWindow", len(window) == 4,
           f"got {len(window)}")
        ok("window keeps the newest", window[-1]["text"] == "a4")

        # --- tools proxied with act-as --------------------------------------
        schemas = {s["name"] for s in provider.get_tool_schemas()}
        ok("tool schemas exposed",
           {"mwe_search", "mwe_dashboard_link", "mwe_disambig_commit"} <= schemas)
        out = json.loads(provider.handle_tool_call("mwe_search", {"query": "serra"}))
        ok("search proxied", out == {"hits": []})
        search = stub.calls("wiki_search")
        ok("search carries act-as", search[0]["headers"].get("x-mwe-act-as") == "anna")
        json.loads(provider.handle_tool_call("mwe_dashboard_link", {}))
        ok("dashboard link proxied",
           stub.calls("dashboard_link")[0]["arguments"]["intent"] == "home")

        # --- disambiguation follow-up ----------------------------------------
        stub.responses["wiki_ingest_message"] = dict(
            stub.responses["wiki_ingest_message"],
            needs_disambig=True,
            disambig_candidates=[{"candidate_id": "c1", "description": "la serra"},
                                 {"candidate_id": "c2", "description": "il garage"}],
        )
        block = provider.prefetch("buttala")
        ok("disambig surfaced to the agent",
           "mwe_disambig_commit" in block and "c1" in block and "c2" in block)
        stub.responses["wiki_ingest_message"] = dict(
            stub.responses["wiki_ingest_message"], needs_disambig=False,
            disambig_candidates=[],
        )
        json.loads(provider.handle_tool_call("mwe_disambig_commit", {"candidate_id": "c1"}))
        commit = stub.calls("wiki_ingest_message")[-1]
        ok("disambig committed with choice + original text",
           commit["arguments"]["metadata"]["disambig_choice"] == "c1"
           and commit["arguments"]["text"] == "buttala")
        out = json.loads(provider.handle_tool_call("mwe_disambig_commit", {"candidate_id": "c1"}))
        ok("stale commit rejected", "error" in out)

        # --- built-in memory one-way mirror -----------------------------------
        before = len(stub.calls("wiki_ingest_message"))
        provider.on_memory_write("add", "user", "preferisce il caffè senza zucchero")
        provider.on_memory_write("replace", "memory", "il curatore gira ogni notte")
        provider.shutdown()  # joins the mirror threads
        mirrors = stub.calls("wiki_ingest_message")[before:]
        ok("both mirror writes ingested", len(mirrors) == 2, f"got {len(mirrors)}")
        by_target = {m["headers"].get("x-mwe-act-as", ""): m for m in mirrors}
        ok("user-target write acts as the human", "anna" in by_target)
        ok("memory-target write is the bot itself", "" in by_target)
        ok("mirror uses the import hint",
           all(m["arguments"]["context_hint"] == "import" for m in mirrors))

        # --- per-sender attribution from the gateway ---------------------------
        gw = fresh_provider(stub.url, platform="telegram", user_id="42")
        gw.prefetch("messaggio dal gateway")
        ok("senderMap routes the gateway sender",
           stub.calls("wiki_ingest_message")[-1]["headers"].get("x-mwe-act-as") == "bruno")

        # --- media: gateway hook → upload → spool → attachments ----------------
        # The media half loads through hermes's REAL general plugin
        # manager (opt-in via plugins.enabled, written above) and fires
        # on the documented pre_gateway_dispatch seam with a REAL
        # MessageEvent.
        import time as _time

        from gateway.config import Platform
        from gateway.platforms.base import MessageEvent, MessageType
        from gateway.session import SessionSource
        from hermes_cli.plugins import get_plugin_manager

        mgr = get_plugin_manager()
        mgr.discover_and_load(force=True)
        plugin_info = {p["key"]: p for p in mgr.list_plugins()}
        ok("media hook plugin enabled via plugins.enabled",
           plugin_info.get("mwe-media", {}).get("enabled") is True,
           f"got {plugin_info.get('mwe-media')}")
        # The memory half sits in the same user-plugins dir: the general
        # manager must classify it as exclusive and leave it to the
        # memory discovery path, not load it as a hook plugin.
        ok("memory half not hijacked by the general plugin manager",
           plugin_info.get("mwe", {}).get("enabled") is False,
           f"got {plugin_info.get('mwe')}")

        photo = HOME / "fixture-photo.jpg"
        photo.write_bytes(b"\xff\xd8\xff" + b"jpegbytes")
        spool = HOME / "mwe-media-spool.json"

        def media_event(user_id, text="la serra al tramonto"):
            return MessageEvent(
                text=text,
                message_type=MessageType.PHOTO,
                source=SessionSource(platform=Platform.TELEGRAM,
                                     chat_id="chat1", user_id=user_id),
                media_urls=[str(photo)],
                media_types=["image/jpeg"],
            )

        def fire_hook(event):
            return mgr.invoke_hook("pre_gateway_dispatch", event=event,
                                   gateway=None, session_store=None)

        # (a) mapped sender: exactly one POST /media, bearer + act-as, spooled
        results = fire_hook(media_event("42"))
        ok("hook answers allow",
           any(isinstance(r, dict) and r.get("action") == "allow" for r in results))
        ups = stub.media_uploads
        ok("mapped sender → exactly one POST /media", len(ups) == 1, f"got {len(ups)}")
        ok("upload carries the bearer token",
           ups[0]["headers"].get("authorization") == "Bearer test-jwt")
        ok("upload act-as = the senderMap-mapped sender",
           ups[0]["headers"].get("x-mwe-act-as") == "bruno")
        ok("upload kind + caption + bytes",
           ups[0]["kind"] == "photo"
           and ups[0]["caption"] == "la serra al tramonto"
           and ups[0]["filename"] == "fixture-photo.jpg"
           and ups[0]["size"] == photo.stat().st_size)
        ok("spool file written", spool.is_file() and len(json.loads(spool.read_text())) == 1)

        # (b) UNMAPPED sender: zero uploads (fail-closed — the hook fires
        # before hermes's own authorization, primaryUser does not apply)
        fire_hook(media_event("99"))
        ok("unmapped sender uploads nothing (fail-closed)",
           len(stub.media_uploads) == 1, f"got {len(stub.media_uploads)}")

        # spool isolation: another sender's prefetch must not drain the entry
        provider.prefetch("turno del primario, senza media")
        ok("other sender's ingest carries no attachments",
           "attachments" not in stub.calls("wiki_ingest_message")[-1]["arguments"])
        ok("entry survives the other sender's drain",
           len(json.loads(spool.read_text())) == 1)

        # (c) the mapped sender's ingest carries the catalog id, then drains
        gw.prefetch("la serra al tramonto")
        ing = stub.calls("wiki_ingest_message")[-1]
        atts = ing["arguments"].get("attachments")
        ok("attachments ride the ingest",
           bool(atts) and atts[0]["catalog_id"] == "c-2026-06-12-photo-001.jpg",
           f"got {atts}")
        ok("attachment kind + caption forwarded",
           atts[0]["kind"] == "photo" and atts[0].get("caption") == "la serra al tramonto")
        ok("media ingest act-as stays the mapped sender",
           ing["headers"].get("x-mwe-act-as") == "bruno")
        gw.prefetch("turno successivo senza media")
        ok("spool drained — next ingest clean",
           "attachments" not in stub.calls("wiki_ingest_message")[-1]["arguments"])

        # (d) native image mode: hermes hands the provider an empty query
        # on media turns; spooled attachments must still fire ONE ingest
        fire_hook(media_event("42", text="tramonto sul lago"))
        n_ing = len(stub.calls("wiki_ingest_message"))
        block = gw.prefetch("")
        ok("native turn fires exactly one ingest",
           len(stub.calls("wiki_ingest_message")) == n_ing + 1)
        native = stub.calls("wiki_ingest_message")[-1]
        ok("native turn text = the spooled caption",
           native["arguments"]["text"] == "tramonto sul lago")
        ok("native turn carries the attachment",
           native["arguments"]["attachments"][0]["catalog_id"]
           == "c-2026-06-12-photo-002.jpg")
        ok("native turn still returns the recall block", "stub" in block)
        fire_hook(media_event("42", text=""))  # captionless photo
        gw.prefetch("")
        ok("captionless native turn falls back to [media]",
           stub.calls("wiki_ingest_message")[-1]["arguments"]["text"] == "[media]")
        n_ing = len(stub.calls("wiki_ingest_message"))
        ok("empty text + empty spool stays inert",
           gw.prefetch("") == ""
           and len(stub.calls("wiki_ingest_message")) == n_ing)

        # the mwe.json knobs: per-file size cap + media kill-switch
        n_ups = len(stub.media_uploads)
        stub_mwe_json(mediaMaxBytes=4)
        fire_hook(media_event("42"))
        ok("size cap skips the oversized file", len(stub.media_uploads) == n_ups)
        stub_mwe_json(mediaEnabled=False)
        fire_hook(media_event("42"))
        ok("mediaEnabled=false kill-switch uploads nothing",
           len(stub.media_uploads) == n_ups)
        stub_mwe_json()  # restore
        ok("knob runs left no spool entries",
           json.loads(spool.read_text()) == [])

        # (e) a stale spool entry (older than the TTL) is dropped at read
        spool.write_text(json.dumps([{
            "gateway_key": "telegram:42",
            "attachments": [{"catalog_id": "c-2026-06-12-photo-099.jpg",
                             "kind": "photo"}],
            "caption": "vecchia foto", "ts": _time.time() - 3600,
        }]))
        n_ing = len(stub.calls("wiki_ingest_message"))
        ok("stale spool entry dropped (no ingest)",
           gw.prefetch("") == ""
           and len(stub.calls("wiki_ingest_message")) == n_ing)
        ok("stale entry purged from the spool file",
           json.loads(spool.read_text()) == [])

        # the disambig commit drains the spool too
        stub.responses["wiki_ingest_message"] = dict(
            stub.responses["wiki_ingest_message"],
            needs_disambig=True,
            disambig_candidates=[{"candidate_id": "m1", "description": "la foto"}],
        )
        gw.prefetch("ricordati questa foto")
        stub.responses["wiki_ingest_message"] = dict(
            stub.responses["wiki_ingest_message"],
            needs_disambig=False, disambig_candidates=[],
        )
        fire_hook(media_event("42", text="la cassetta rossa"))
        json.loads(gw.handle_tool_call("mwe_disambig_commit", {"candidate_id": "m1"}))
        commit = stub.calls("wiki_ingest_message")[-1]
        ok("disambig commit drains spooled attachments",
           commit["arguments"]["metadata"]["disambig_choice"] == "m1"
           and commit["arguments"]["attachments"][0]["catalog_id"]
           == "c-2026-06-12-photo-004.jpg",
           f"got {commit['arguments'].get('attachments')}")

        # (f) a mixed photo+video album: hermes merges it into ONE event
        # whose message_type reflects only the first item — the kind must
        # come from each file's mime, or the photo would be catalogued as
        # 'video' and silently skip the server's vision path.
        clip = HOME / "fixture-clip.mp4"
        clip.write_bytes(b"\x00\x00\x00\x18ftypmp42" + b"mp4bytes")
        album = MessageEvent(
            text="album misto",
            message_type=MessageType.VIDEO,
            source=SessionSource(platform=Platform.TELEGRAM,
                                 chat_id="chat1", user_id="42"),
            media_urls=[str(clip), str(photo)],
            media_types=["video/mp4", "image/jpeg"],
        )
        n_ups = len(stub.media_uploads)
        fire_hook(album)
        ups = stub.media_uploads[n_ups:]
        ok("mixed album uploads every file", len(ups) == 2, f"got {len(ups)}")
        ok("mixed album kinds come from the per-file mime",
           [u["kind"] for u in ups] == ["video", "photo"],
           f"got {[u['kind'] for u in ups]}")
        entry = json.loads(spool.read_text())[-1]
        ok("spool entry carries the per-file kinds",
           [a["kind"] for a in entry["attachments"]] == ["video", "photo"])
        gw.prefetch("ecco l'album")
        atts = stub.calls("wiki_ingest_message")[-1]["arguments"]["attachments"]
        ok("album attachments ride one ingest, kinds intact",
           [a["kind"] for a in atts] == ["video", "photo"], f"got {atts}")

        # (g) the aggregate per-event upload budget: each file is bounded
        # by the upload timeout, but a multi-item album against a hung
        # server would stall the gateway for files × timeout. Budget 0 is
        # exhausted before the first file — the album uploads only what
        # fits (here: nothing), spools nothing, and never raises.
        n_ups = len(stub.media_uploads)
        stub_mwe_json(mediaUploadBudgetSeconds=0)
        results = fire_hook(album)
        ok("budget-exhausted hook still answers allow",
           any(isinstance(r, dict) and r.get("action") == "allow" for r in results))
        ok("budget 0 uploads at most what fits (nothing) and never raises",
           len(stub.media_uploads) == n_ups, f"got {len(stub.media_uploads) - n_ups}")
        ok("budget-skipped album leaves no spool entries",
           json.loads(spool.read_text()) == [])
        stub_mwe_json()  # restore

        # --- non-primary contexts are fully inactive ---------------------------
        sub = fresh_provider(stub.url, agent_context="subagent")
        n_before = len(stub.calls())
        ok("subagent prefetch inert", sub.prefetch("query subagente") == "")
        sub.on_memory_write("add", "user", "non deve arrivare")
        sub.shutdown()
        ok("subagent makes zero server calls", len(stub.calls()) == n_before)

    # --- degradation: dead endpoint, turn survives -----------------------------
    block = provider.prefetch("il server è giù")
    ok("degrades to empty block, no exception", block == "")

    # --- the truncating engine ---------------------------------------------------
    engine.update_model(model="m", context_length=1000)
    engine.update_from_response({"prompt_tokens": 800, "completion_tokens": 10})
    ok("should_compress above threshold", engine.should_compress())
    ok("should_compress below threshold", not engine.should_compress(prompt_tokens=100))
    msgs = [{"role": "system", "content": "sys"}]
    msgs += [{"role": "user", "content": f"m{i}"} if i % 2 == 0
             else {"role": "assistant", "content": f"m{i}"} for i in range(40)]
    out = engine.compress(list(msgs))
    ok("system kept", out[0]["role"] == "system")
    ok("middle dropped, no summary added",
       len(out) < len(msgs) and all(m in msgs for m in out))
    non_system = [m for m in out if m["role"] != "system"]
    ok("head + tail preserved",
       non_system[0]["content"] == "m0"
       and non_system[-1]["content"] == "m39"
       and len(non_system) == 3 + 16)
    ok("tail starts on a user message",
       non_system[3]["content"] == "m24" and non_system[3]["role"] == "user")

    # tool-pairing safety: no kept tool result may lose its call (or vice versa)
    def no_orphans(kept):
        for j, m in enumerate(kept):
            if m["role"] == "tool":
                if j == 0 or not kept[j - 1].get("tool_calls"):
                    return False
            if m.get("tool_calls"):
                if j + 1 >= len(kept) or kept[j + 1]["role"] != "tool":
                    return False
        return True

    msgs2 = [{"role": "system", "content": "sys"}]
    for i in range(12):
        msgs2 += [{"role": "user", "content": f"u{i}"},
                  {"role": "assistant", "content": "", "tool_calls": [{"id": f"t{i}"}]},
                  {"role": "tool", "content": f"r{i}"},
                  {"role": "assistant", "content": f"a{i}"}]
    out2 = engine.compress(list(msgs2))
    kept2 = [m for m in out2 if m["role"] != "system"]
    ok("no orphan tool pairing at the cut", no_orphans(kept2),
       f"kept sequence: {[(m['role'], bool(m.get('tool_calls'))) for m in kept2[:6]]}")

    print(f"\nhermes bridge offline smoke OK — {PASSED} assertions")


if __name__ == "__main__":
    main()
