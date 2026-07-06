"""mwe media hook for hermes-agent — the mwe-mcp bridge, media half.

Without this plugin the bridge silently drops Telegram media: at most a
caption survives the turn. This plugin closes that hole on hermes's
documented `pre_gateway_dispatch` seam — fired once per incoming
`MessageEvent`, *before* hermes's own authorization and agent dispatch.
By the time the hook fires, hermes has already downloaded the bytes into
its local cache via the bot token (`event.media_urls` holds local file
paths; `event.media_types` the mime strings).

Entry is two-phase, mirroring the server design
(the media-pipeline design note): the bytes travel out of band via
`POST <origin>/media` (multipart, same bearer + act-as as `/mcp`), and
the minted catalog ids then ride `wiki_ingest_message.attachments` on
the per-turn ingest performed by the bridge's memory half. The two
halves live in different plugin module namespaces, so a JSON spool file
under `$HERMES_HOME` is the channel between them: this hook appends
`{gateway_key, attachments, caption, ts}` entries; the memory half
drains the entries for its sender on the next `prefetch()`. The two
sides run on different threads (event loop vs agent executor), so each
read-modify-write holds an exclusive flock on the sidecar
`<spool>.lock` file (`_spool_lock`, duplicated in both halves). Entries
older than the TTL are dropped at read AND write time — a turn that
never fired must not leak attachments into an unrelated later turn.

Three rules govern this hook:

- **Fail-closed.** The hook runs BEFORE hermes authorizes the sender, so
  an unauthorized stranger's bytes must never reach the memory server.
  Uploads happen only for senders with an *explicit* `senderMap` entry
  in `mwe.json`; the memory half's `primaryUser` fallback deliberately
  does NOT apply here.
- **Degradation never kills the turn.** Any failure (config missing,
  upload error, spool unwritable) logs a warning and lets the event
  flow on untouched — same contract as the memory half.
- **Synchronous, bounded.** Uploads run inline with a modest timeout:
  the gateway event loop is waiting, and the ingest that consumes the
  catalog ids fires right after dispatch.

Opt-in: hermes loads this plugin only when `mwe-media` appears in
`plugins.enabled` in `config.yaml` (standalone plugins are opt-in by
default). Config rides the bridge's `mwe.json` (`senderMap`, plus the
optional `mediaEnabled` / `mediaMaxBytes` knobs) and the `MWE_TOKEN`
secret in the environment. Stdlib-only — no pip dependencies.

UPSTREAM TRAP — do not mention the memory-provider registration symbols
(the ABC class name or the `register_…` method that installs one) in
this file, not even in comments: hermes sniffs the first 8 KiB of every
user plugin's `__init__.py` for those two literals and silently coerces
any match to an exclusive memory plugin that the hook system never
loads (`hermes_cli/plugins.py::_parse_manifest`). The explicit
`kind: standalone` in `plugin.yaml` is the second lock on that door.
"""

from __future__ import annotations

import contextlib
import fcntl
import importlib.util
import json
import logging
import os
import time
from pathlib import Path
from typing import Any, Dict, List, Optional

logger = logging.getLogger(__name__)

# Spool protocol shared with the memory half
# (plugins/memory/mwe/__init__.py) — keep the two constants and the
# `_spool_lock` twin helper in sync.
SPOOL_FILENAME = "mwe-media-spool.json"
SPOOL_TTL_SECONDS = 180.0

# Default per-file size cap. Telegram bots cap downloads at 20 MiB and
# the server rejects bodies above 32 MiB — checked on st_size BEFORE
# reading bytes. Override via `mediaMaxBytes` in mwe.json.
DEFAULT_MAX_BYTES = 20 * 1024 * 1024

# Upload timeout: the hook runs synchronously on the gateway event loop,
# so a hung server must not stall message dispatch for long. Bounds each
# FILE; the per-event bound is DEFAULT_UPLOAD_BUDGET_SECONDS below.
UPLOAD_TIMEOUT_SECONDS = 30

# Aggregate per-event upload budget: the per-file timeout alone lets a
# multi-file album against a black-holed server stall the gateway event
# loop for files × timeout. Once the budget is spent, the remaining
# files are skipped with one warning (the degradation contract already
# covers skipped files). Override via `mediaUploadBudgetSeconds` in
# mwe.json.
DEFAULT_UPLOAD_BUDGET_SECONDS = 60.0

# hermes MessageType.value → the server's catalog kind enum
# (photo | video | audio | doc) — the EVENT-level kind. It gates which
# message types enter the pipeline at all; the upload loop refines the
# kind per file from the parallel `media_types` mime (`_kind_for_mime`),
# because mixed albums merge into one event. Types not listed (text,
# sticker, location, command) carry no uploadable media for this
# pipeline.
KIND_BY_MESSAGE_TYPE = {
    "photo": "photo",
    "video": "video",
    "audio": "audio",
    "voice": "audio",
    "document": "doc",
}

# Per-file mime prefix → catalog kind (see _kind_for_mime).
KIND_BY_MIME_PREFIX = {"image": "photo", "video": "video", "audio": "audio"}

# Lazily loaded vendored MweClient module (memory half's client.py).
_client_module: Any = None
_client_module_error = False


def _hermes_home() -> Path:
    """Resolve $HERMES_HOME through hermes's own helper when available."""
    try:
        from hermes_constants import get_hermes_home

        return Path(get_hermes_home())
    except Exception:
        return Path(os.environ.get("HERMES_HOME") or (Path.home() / ".hermes"))


def _load_client_module() -> Any:
    """Import the vendored `MweClient` from the memory half by file path.

    The two plugins live in different module namespaces, so the client
    is reached on disk instead of via `import`. Candidate locations:

    1. installed layout — `$HERMES_HOME/plugins/mwe/client.py`, the
       sibling symlink the bridge install creates (resolved relative to
       this file's *unresolved* path, so the symlinked siblings line up);
    2. in-repo layout — `agents-bridges/hermes/plugins/memory/mwe/client.py`
       relative to this file's *resolved* path (also covers the
       installed-via-symlink case, since the symlink resolves into the
       repo checkout).
    """
    global _client_module, _client_module_error
    if _client_module is not None or _client_module_error:
        return _client_module
    here = Path(__file__)
    candidates = [
        here.parent.parent / "mwe" / "client.py",
        here.resolve().parent.parent.parent / "memory" / "mwe" / "client.py",
    ]
    for candidate in candidates:
        if not candidate.is_file():
            continue
        try:
            spec = importlib.util.spec_from_file_location(
                "hermes_plugins.mwe_media._vendored_client", str(candidate)
            )
            if spec is None or spec.loader is None:
                continue
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
            _client_module = module
            return _client_module
        except Exception as e:
            logger.warning("mwe-media: client at %s unloadable: %s", candidate, e)
    _client_module_error = True
    logger.warning(
        "mwe-media: vendored mwe client not found (looked in %s) — media "
        "uploads disabled; is the memory half installed as a sibling plugin?",
        ", ".join(str(c) for c in candidates),
    )
    return None


def _load_bridge_config(home: Path) -> Dict[str, Any]:
    """Read `$HERMES_HOME/mwe.json` (the bridge's shared config file)."""
    path = home / "mwe.json"
    if not path.is_file():
        return {}
    try:
        data = json.loads(path.read_text())
        return data if isinstance(data, dict) else {}
    except Exception as e:
        logger.warning("mwe-media: mwe.json unreadable (%s) — hook inactive", e)
        return {}


def _kind_for_mime(mime: Optional[str], event_kind: str) -> str:
    """Derive one file's catalog kind from its mime.

    hermes merges a mixed photo+video Telegram album into ONE event
    whose `media_types` list is per-file while `message_type` reflects
    only the first item — deriving the kind once per event would stamp
    e.g. 'video' on a photo, which silently skips the server's vision
    path. The per-file mime wins; the event-level kind covers files
    with no usable mime; `doc` is the last resort.
    """
    prefix = (mime or "").split("/", 1)[0].strip().lower()
    return KIND_BY_MIME_PREFIX.get(prefix) or event_kind or "doc"


@contextlib.contextmanager
def _spool_lock(path: Path):
    """Exclusive flock held across one spool read-modify-write.

    TWIN — duplicated verbatim in the bridge's other half
    (plugins/gateway/mwe-media/__init__.py ↔ plugins/memory/mwe/
    __init__.py): the two plugins live in different module namespaces
    and must stay self-contained; keep the copies textually identical.

    The gateway hook appends on the gateway event-loop thread while the
    memory provider drains on an agent worker thread (hermes offloads
    the turn to an executor), so the two read→filter→replace cycles can
    interleave and lose or resurrect entries. The lock lives on a
    sidecar file (`<spool>.lock`) because the spool itself is swapped by
    `os.replace` — an flock on the replaced inode would guard nothing.
    Lock/IO failures propagate to the caller, whose degradation contract
    applies (hook: skip spooling; drain: no attachments); the lock file
    is empty, persistent, and safe to delete alongside the spool.
    """
    lock_path = path.with_name(path.name + ".lock")
    with open(lock_path, "a") as lock_file:
        fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(lock_file.fileno(), fcntl.LOCK_UN)


def _spool_append(home: Path, entry: Dict[str, Any]) -> None:
    """Append one entry to the spool file, pruning stale entries.

    Pruning at write time keeps the file from growing when no memory
    half ever drains it (e.g. the provider is not configured). The
    whole read-modify-write holds the sidecar flock (`_spool_lock`) so
    the memory half's concurrent drain on an agent worker thread cannot
    interleave its own read→replace and drop or resurrect entries; the
    write stays atomic (`os.replace`) so even a lockless reader never
    sees a torn file. Failures propagate to the hook's catch-all: a
    logged warning, no spooling, the turn untouched.
    """
    path = home / SPOOL_FILENAME
    with _spool_lock(path):
        entries: List[Dict[str, Any]] = []
        if path.is_file():
            try:
                existing = json.loads(path.read_text())
                if isinstance(existing, list):
                    entries = [e for e in existing if isinstance(e, dict)]
            except Exception:
                entries = []
        now = time.time()

        def _fresh(e: Dict[str, Any]) -> bool:
            try:
                return (now - float(e.get("ts", 0))) <= SPOOL_TTL_SECONDS
            except (TypeError, ValueError):
                return False

        entries = [e for e in entries if _fresh(e)]
        entries.append(entry)
        tmp = path.with_name(path.name + ".hook.tmp")
        tmp.write_text(json.dumps(entries))
        os.replace(tmp, path)


def _process_media(event: Any) -> None:
    """Upload an event's media and spool the catalog ids (may raise)."""
    media_urls = list(getattr(event, "media_urls", None) or [])
    if not media_urls:
        return

    source = getattr(event, "source", None)
    platform = str(getattr(getattr(source, "platform", None), "value", "") or "")
    user_id = str(getattr(source, "user_id", "") or "")
    if not platform or not user_id:
        # No attributable sender (service messages, anonymous posts):
        # nothing to act-as — fail closed.
        return

    home = _hermes_home()
    cfg = _load_bridge_config(home)
    url = (cfg.get("url") or "").strip()
    token = os.environ.get("MWE_TOKEN", "").strip()
    if not url or not token:
        return  # the bridge is not configured — stay inert
    if cfg.get("mediaEnabled") is False:
        return  # operator kill-switch (defaults on)

    # Fail-closed sender mapping: the hook fires before hermes's own
    # authorization, so only an EXPLICIT senderMap entry may upload —
    # the memory half's primaryUser fallback does not apply to media.
    gateway_key = f"{platform}:{user_id}"
    sender_map = cfg.get("senderMap") or {}
    act_as = sender_map.get(gateway_key) or sender_map.get(user_id)
    if not act_as:
        logger.info(
            "mwe-media: sender %s has no senderMap entry — media not "
            "uploaded (fail-closed)", gateway_key,
        )
        return

    message_type = getattr(event, "message_type", None)
    type_value = str(getattr(message_type, "value", message_type) or "")
    event_kind = KIND_BY_MESSAGE_TYPE.get(type_value)
    if event_kind is None:
        logger.debug("mwe-media: message type %r carries no media kind", type_value)
        return

    client_module = _load_client_module()
    if client_module is None:
        return

    try:
        max_bytes = int(cfg.get("mediaMaxBytes", DEFAULT_MAX_BYTES))
    except (TypeError, ValueError):
        max_bytes = DEFAULT_MAX_BYTES
    try:
        budget = float(
            cfg.get("mediaUploadBudgetSeconds", DEFAULT_UPLOAD_BUDGET_SECONDS)
        )
    except (TypeError, ValueError):
        budget = DEFAULT_UPLOAD_BUDGET_SECONDS

    caption = (getattr(event, "text", "") or "").strip()
    media_types = list(getattr(event, "media_types", None) or [])
    client = client_module.MweClient(
        url, token, act_as=str(act_as), timeout=UPLOAD_TIMEOUT_SECONDS
    )

    deadline = time.monotonic() + budget
    attachments: List[Dict[str, Any]] = []
    for i, path_str in enumerate(media_urls):
        if time.monotonic() >= deadline:
            # The per-file timeout bounds each upload, but a multi-file
            # album against a hung server would stall the gateway event
            # loop for files × timeout — the aggregate budget cuts the
            # loop instead (one warning; skipped files are the ordinary
            # degradation).
            logger.warning(
                "mwe-media: per-event upload budget (%.0f s) spent — "
                "skipping %d remaining file(s) (turn proceeds)",
                budget, len(media_urls) - i,
            )
            break
        path = Path(str(path_str))
        try:
            size = path.stat().st_size
        except OSError as e:
            logger.warning("mwe-media: cached media %s unreadable: %s", path, e)
            continue
        if size <= 0 or size > max_bytes:
            logger.warning(
                "mwe-media: %s is %d bytes (cap %d) — skipped", path, size, max_bytes,
            )
            continue
        mime = media_types[i] if i < len(media_types) else None
        kind = _kind_for_mime(mime, event_kind)
        try:
            resp = client.upload_media(
                str(path), kind, caption=caption or None, mime=mime or None
            )
        except Exception as e:
            logger.warning("mwe-media: upload of %s failed (turn proceeds): %s", path, e)
            continue
        catalog_id = resp.get("catalog_id") if isinstance(resp, dict) else None
        if not catalog_id:
            logger.warning("mwe-media: upload of %s returned no catalog_id", path)
            continue
        attachment: Dict[str, Any] = {
            "catalog_id": catalog_id,
            "kind": resp.get("kind") or kind,
        }
        if caption:
            attachment["caption"] = caption
        attachments.append(attachment)
        logger.info(
            "mwe-media: uploaded %s as %s (act-as %s, dedup=%s)",
            path.name, catalog_id, act_as, resp.get("dedup"),
        )

    if attachments:
        _spool_append(home, {
            "gateway_key": gateway_key,
            "attachments": attachments,
            "caption": caption,
            "ts": time.time(),
        })


def on_pre_gateway_dispatch(event: Any = None, **_kwargs: Any) -> Optional[Dict[str, Any]]:
    """The `pre_gateway_dispatch` callback: upload + spool, then allow.

    Always answers `{"action": "allow"}` — this hook observes media, it
    never drops or rewrites the message; degradation on any failure is a
    logged warning and an untouched event.
    """
    try:
        if event is not None:
            _process_media(event)
    except Exception as e:
        logger.warning("mwe-media hook degraded — turn proceeds without media: %s", e)
    return {"action": "allow"}


def register(ctx) -> None:
    ctx.register_hook("pre_gateway_dispatch", on_pre_gateway_dispatch)
