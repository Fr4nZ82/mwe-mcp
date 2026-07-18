"""mwe-watchdog for hermes-agent — the mwe-mcp bridge, verification half.

The per-turn contract has a blind spot on the host side: the memory
provider's `prefetch()` returns the recall block, but whether hermes
actually injects it into the outgoing model request is invisible to the
provider. A host-side drop is silent by construction — the turn
proceeds, the model answers without memory, and nothing logs. Observed
live (2026-07-18): hermes's message-alternation repair compacts the
transcript AFTER the injection index was computed, so on an affected
chat every `<memory-context>` block was discarded for days while
capture kept working — a memory blackout with zero signal.

This plugin closes the loop, zero fork:

- The memory half records what it handed the host for the current turn
  (`mwe-watchdog-state.json` under `$HERMES_HOME`, keyed by a hash of
  the turn's user text — the file is the channel, twin note there).
- This hook fires on hermes's documented `pre_api_request` seam with
  the sanitised outgoing request. On the turn's FIRST model call it
  matches the state entry and checks a `<memory-context>` fence is
  present in a user message. Present → reset the miss counter (log the
  recovery if one was running). Absent → a loud WARNING with a
  consecutive-miss counter; from the third consecutive miss the message
  flags the drop as SYSTEMATIC and points at the known host pathologies
  (message repair / preflight compression shifting the injection index).

Diagnosis only — it never mutates the request and never touches the
turn. Failures degrade to silence, same contract as the other halves.
"""

from __future__ import annotations

import contextlib
import fcntl
import hashlib
import json
import logging
import os
import time
from pathlib import Path
from typing import Any, Dict, List, Optional

logger = logging.getLogger(__name__)

# Twin constants — keep in sync with plugins/memory/mwe/__init__.py.
_STATE_FILENAME = "mwe-watchdog-state.json"
_MATCH_TTL_SECONDS = 300.0

# The fence hermes wraps injected memory in (agent/memory_manager.py,
# build_memory_context_block). Presence in any user message of the
# outgoing request is the proof of delivery.
_FENCE = "<memory-context>"

# Consecutive misses from which the warning calls the drop systematic.
_SYSTEMATIC_AFTER = 3


def _hermes_home() -> Path:
    try:
        from hermes_constants import get_hermes_home
        return Path(get_hermes_home())
    except Exception:
        return Path(os.environ.get("HERMES_HOME", str(Path.home() / ".hermes")))


@contextlib.contextmanager
def _state_lock(path: Path):
    """Exclusive flock across one state read-modify-write.

    TWIN of `_spool_lock` in the bridge's other halves (the plugins live
    in different module namespaces and must stay self-contained). The
    lock lives on a sidecar file because the state file itself is
    swapped by `os.replace` — an flock on the replaced inode would
    guard nothing.
    """
    lock_path = path.with_name(path.name + ".lock")
    with open(lock_path, "a") as lock_file:
        fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(lock_file.fileno(), fcntl.LOCK_UN)


def _flatten(content: Any) -> str:
    """Text view of a message content — str passes, typed parts join."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts: List[str] = []
        for p in content:
            if isinstance(p, dict) and isinstance(p.get("text"), str):
                parts.append(p["text"])
            elif isinstance(p, str):
                parts.append(p)
        return " ".join(parts)
    return ""


def _check(kwargs: Dict[str, Any]) -> None:
    if kwargs.get("api_call_count") != 1:
        return  # one verification per turn, on the first model call
    user_message = kwargs.get("user_message")
    if not isinstance(user_message, str) or not user_message.strip():
        return  # multimodal / scaffolded turns carry no comparable key
    path = _hermes_home() / _STATE_FILENAME
    if not path.is_file():
        return
    sha = hashlib.sha256(user_message.encode("utf-8")).hexdigest()[:16]
    now = time.time()
    with _state_lock(path):
        try:
            data = json.loads(path.read_text())
        except Exception:
            return
        if not isinstance(data, dict):
            return
        match: Optional[Dict[str, Any]] = next(
            (
                e
                for e in reversed(data.get("entries") or [])
                if isinstance(e, dict)
                and e.get("query_sha") == sha
                and now - float(e.get("ts", 0) or 0) < _MATCH_TTL_SECONDS
            ),
            None,
        )
        if match is None:
            return  # no handshake for this turn (empty block, media turn, …)
        request = kwargs.get("request") or {}
        body = request.get("body") if isinstance(request, dict) else None
        messages = body.get("messages") if isinstance(body, dict) else None
        if not isinstance(messages, list):
            return  # request shape unknown — no verdict is better than a false one
        delivered = any(
            isinstance(m, dict)
            and m.get("role") == "user"
            and _FENCE in _flatten(m.get("content"))
            for m in messages
        )
        if delivered:
            prior = int(data.get("consecutive_misses") or 0)
            if prior:
                logger.info(
                    "mwe-watchdog: recall block delivery recovered after %s "
                    "missed turn(s)", prior,
                )
            data["consecutive_misses"] = 0
        else:
            misses = int(data.get("consecutive_misses") or 0) + 1
            data["consecutive_misses"] = misses
            data["last_miss_ts"] = now
            logger.warning(
                "mwe-watchdog: the recall block handed to the host for this "
                "turn (%s chars, sender=%s) is MISSING from the outgoing model "
                "request (model=%s, session=%s) — host-side injection dropped "
                "it; the model is answering WITHOUT memory. "
                "consecutive_misses=%s%s",
                match.get("block_chars"),
                match.get("sender"),
                kwargs.get("model"),
                kwargs.get("session_id"),
                misses,
                (
                    " — SYSTEMATIC: every recent turn lost its memory. Suspect "
                    "a host transcript pathology (message-alternation repair / "
                    "preflight compression shifting the injection index after "
                    "it was computed); a clean session reset while the chat is "
                    "idle usually clears it."
                    if misses >= _SYSTEMATIC_AFTER
                    else ""
                ),
            )
        tmp = path.with_name(path.name + ".watchdog.tmp")
        tmp.write_text(json.dumps(data, ensure_ascii=False))
        os.replace(tmp, path)


def on_pre_api_request(**kwargs: Any) -> None:
    """The `pre_api_request` callback: verify, never interfere."""
    try:
        _check(kwargs)
    except Exception as e:
        logger.debug("mwe-watchdog: check failed (non-fatal): %s", e)
    return None


def register(ctx) -> None:
    ctx.register_hook("pre_api_request", on_pre_api_request)
    logger.info(
        "mwe-watchdog: registered on pre_api_request "
        "(per-turn contract verification)"
    )
