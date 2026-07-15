"""mwe-truncate context engine for hermes-agent — the mwe-mcp bridge, context half.

The sessionless model of the mwe-mcp per-turn contract (v1): the bounded
recent window is the only conversational state hermes keeps; long-range
continuity comes from the per-turn recall block (`memory.provider: mwe`),
not from a compression summary. `compress()` therefore **truncates** — it
keeps the system messages, an optional small protected head, and the tail
from the Nth-most-recent **user** message onward, and drops the middle
with **no summarization pass** (no LLM call, no summary message).

The window is counted in user messages, not raw messages: with tool use a
single user turn fans out into several assistant/tool messages, so a raw
message count keeps an unpredictable number of turns. Cutting at a
user-message boundary also keeps assistant tool-calls paired with their
tool results by construction.

Two knobs shape the cut. `protect_last_users` is the window kept after a
cut; `slack_users` is how many extra user turns may accumulate before the
cut fires. The slack keeps the prompt prefix stable between cuts, so
provider-side prefix caching keeps working instead of missing on every
turn.

Tool-result snipping: turns dominated by tool spam (browser snapshots,
console dumps) can make even the bounded window exceed the trigger — the
window bounds *turns*, not *weight*. So a compress pass also snips
oversized tool-result contents (`snip_tool_chars`) inside the kept
window, copy-on-write, sparing the tail from the last user message onward
(a mid-loop fire must never snip a result the model is still acting on).
Originals are never mutated: the rotation path archives the
pre-compression list, which keeps the full contents. A snip-only pass
(nothing to drop) only counts as a compression when it saves enough to
clear the host's material-progress bar; otherwise it reports a no-op
through the abort protocol below.

The trigger: the host only passes token counts to `should_compress()`, so
the engine fires on a token threshold — `min(threshold_percent × context
window, threshold_tokens_cap)`. The cap keeps the trigger orders of
magnitude below provider per-minute token quotas even on million-token
context models (0.75 × 1M would otherwise let the conversation grow to
~786k tokens per call before the first cut). When a fire finds nothing to
drop outside the protected window and nothing worth snipping inside it,
the engine reports the no-op through the host's abort protocol
(`_last_compress_aborted`) so the host skips its session-rotation
bookkeeping.

⚠️ Session mode: this engine REQUIRES rotation mode — leave
`compression.in_place` at its vanilla default (false). The in-place
compaction path of hermes-agent (as of 2026-07) is not safe under
preflight fires: after an in-place cut the host nulls the turn's
`conversation_history` and resets its flush-identity bookkeeping, so the
end-of-turn persist re-appends the whole compacted window into the SAME
active transcript — doubling it. The doubled tail replays old user
messages to the model, which then re-answers them (diagnosed 2026-07-15
on a production deployment). Rotation (the vanilla default) makes that
re-append land in the freshly rotated session, where it is the correct
behaviour.

Config (optional), in `config.yaml`:

    context:
      engine: mwe-truncate
      mwe-truncate:
        protect_last_users: 5      # user turns kept after a cut
        slack_users: 3             # extra user turns allowed before a cut fires
        protect_first_n: 0         # non-system messages preserved at the head
        snip_tool_chars: 4000      # tool results above this are snipped on a cut (0 = off)
        threshold_tokens_cap: 30000  # trigger ceiling in prompt tokens (0 = percent-only)
        threshold_percent: 0.75    # share of the model context window
"""

from __future__ import annotations

import logging
from typing import Any, Dict, List, Tuple

from agent.context_engine import ContextEngine

logger = logging.getLogger(__name__)


def _cfg(key: str, default):
    try:
        from hermes_cli.config import cfg_get, load_config_readonly
        value = cfg_get(load_config_readonly(), "context", "mwe-truncate", key)
        return default if value is None else value
    except Exception:
        return default


class MweTruncateContextEngine(ContextEngine):
    """Truncate to a bounded window of user turns; recall replaces the summary."""

    def __init__(self):
        self.threshold_percent = float(_cfg("threshold_percent", 0.75))
        self.threshold_tokens_cap = int(_cfg("threshold_tokens_cap", 30_000))
        self.protect_first_n = int(_cfg("protect_first_n", 0))
        self.protect_last_users = int(_cfg("protect_last_users", 5))
        self.slack_users = int(_cfg("slack_users", 3))
        self.snip_tool_chars = int(_cfg("snip_tool_chars", 4_000))
        # The host preflight gate compares len(messages) against
        # protect_first_n + protect_last_n + 1 before it even estimates
        # tokens; expose the widest window (in user turns — every user
        # turn is at least one message) so the gate never starves a due
        # cut. protect_last_n no longer carries raw-message semantics.
        self.protect_last_n = self.protect_last_users + self.slack_users
        if _cfg("protect_last_n", None) is not None:
            logger.info(
                "mwe-truncate: config key protect_last_n is retired and ignored — "
                "the window is counted in user turns now (protect_last_users/slack_users)"
            )
        self.last_prompt_tokens = 0
        self.last_completion_tokens = 0
        self.last_total_tokens = 0
        self.threshold_tokens = 0
        self.context_length = 0
        self.compression_count = 0
        self.snipped_tool_results = 0
        # Host abort protocol: a truthy _last_compress_aborted after
        # compress() tells conversation_compression to skip the
        # session-rotation bookkeeping and warn instead of rotating.
        self._last_compress_aborted = False
        self._last_summary_error = None

    @property
    def name(self) -> str:
        return "mwe-truncate"

    def is_available(self) -> bool:
        return True

    # -- token tracking ------------------------------------------------------

    def update_from_response(self, usage: Dict[str, Any]) -> None:
        usage = usage or {}
        self.last_prompt_tokens = int(
            usage.get("prompt_tokens") or usage.get("input_tokens") or 0
        )
        self.last_completion_tokens = int(
            usage.get("completion_tokens") or usage.get("output_tokens") or 0
        )
        self.last_total_tokens = int(
            usage.get("total_tokens")
            or (self.last_prompt_tokens + self.last_completion_tokens)
        )

    def update_model(self, model: str, context_length: int, base_url: str = "",
                     api_key: str = "", provider: str = "", api_mode: str = "",
                     **kwargs: Any) -> None:
        # Mirror the base ContextEngine.update_model signature, but accept
        # **kwargs too: hermes-agent extends this call over time (api_mode
        # was the latest add) and this engine only needs context_length, so
        # tolerating unknown keywords keeps the bridge from breaking on the
        # next framework-side parameter.
        self.context_length = int(context_length or 0)
        pct_tokens = int(self.context_length * self.threshold_percent)
        if self.threshold_tokens_cap > 0:
            self.threshold_tokens = (
                min(pct_tokens, self.threshold_tokens_cap) if pct_tokens > 0
                else self.threshold_tokens_cap
            )
        else:
            self.threshold_tokens = pct_tokens

    # -- compaction ------------------------------------------------------------

    def should_compress(self, prompt_tokens: int = None) -> bool:
        tokens = self.last_prompt_tokens if prompt_tokens is None else prompt_tokens
        return self.threshold_tokens > 0 and tokens >= self.threshold_tokens

    def has_content_to_compress(self, messages: List[Dict[str, Any]]) -> bool:
        if self._droppable_indices(messages):
            return True
        _, snipped, _ = self._snip_tool_results(messages)
        return snipped > 0

    def compress(self, messages: List[Dict[str, Any]], current_tokens: int = None,
                 focus_topic: str = None, **kwargs: Any) -> List[Dict[str, Any]]:
        # focus_topic/force are summarization-engine concepts; truncation
        # keeps the same window regardless, and recall surfaces the topic
        # when relevant.
        self._last_compress_aborted = False
        self._last_summary_error = None
        drop = self._droppable_indices(messages)
        kept = [m for i, m in enumerate(messages) if i not in drop]
        kept, snipped, saved_chars = self._snip_tool_results(kept)
        if not drop:
            # The trigger is token-based but the window is turn-based, so a
            # fire can land with nothing outside the window. A snip-only
            # pass must clear the host's material-progress bar (>5% token
            # reduction — chars proxy tokens closely enough here, 8% keeps
            # a margin), or the host would run its session-rotation
            # bookkeeping for a near-no-op. Otherwise report through the
            # abort protocol so the host skips rotation for this fire.
            total_chars = sum(
                len(m.get("content")) for m in messages
                if isinstance(m.get("content"), str)
            )
            if snipped == 0 or total_chars == 0 or saved_chars < total_chars * 0.08:
                self._last_compress_aborted = True
                self._last_summary_error = (
                    "the recent window is already at its turn bound; heavy recent "
                    "turns will age out on their own"
                )
                return messages
        self.compression_count += 1
        self.snipped_tool_results += snipped
        logger.info(
            "mwe-truncate: dropped %d of %d messages, snipped %d tool results "
            "(window: first %d + last %d user turns)",
            len(drop), len(messages), snipped,
            self.protect_first_n, self.protect_last_users)
        return kept

    def get_status(self) -> Dict[str, Any]:
        return {
            "engine": self.name,
            "last_prompt_tokens": self.last_prompt_tokens,
            "threshold_tokens": self.threshold_tokens,
            "context_length": self.context_length,
            "compression_count": self.compression_count,
            "snipped_tool_results": self.snipped_tool_results,
            "protect_first_n": self.protect_first_n,
            "protect_last_users": self.protect_last_users,
            "slack_users": self.slack_users,
            "snip_tool_chars": self.snip_tool_chars,
        }

    # -- internals ---------------------------------------------------------

    def _droppable_indices(self, messages: List[Dict[str, Any]]) -> set:
        """Indices of the middle slice, cut at a user-message boundary.

        Non-system messages keep the first `protect_first_n` and the tail
        from the `protect_last_users`-th-most-recent user message onward;
        everything between is dropped. The cut only fires once more than
        `protect_last_users + slack_users` user messages sit beyond the
        head, and cuts back down to `protect_last_users` — the slack keeps
        the prompt prefix stable between cuts for provider-side caching.
        Because the tail starts on a user message, no kept assistant
        tool-call can lose its tool result. The head is walked so it ends
        on a user message or a plain assistant message for the same
        reason; head messages that fail the check are dropped, not kept.
        """
        non_system = [i for i, m in enumerate(messages) if m.get("role") != "system"]
        head = non_system[:self.protect_first_n]
        body = non_system[len(head):]
        users = [i for i in body if messages[i].get("role") == "user"]
        if len(users) <= self.protect_last_users + self.slack_users:
            return set()
        cut_at = users[-self.protect_last_users]
        droppable = {i for i in body if i < cut_at}

        def is_clean_head_end(idx):
            m = messages[idx]
            return m.get("role") == "user" or (
                m.get("role") == "assistant" and not m.get("tool_calls")
            )

        while head and not is_clean_head_end(head[-1]):
            droppable.add(head.pop())
        return droppable

    def _snip_tool_results(
        self, kept: List[Dict[str, Any]]
    ) -> Tuple[List[Dict[str, Any]], int, int]:
        """Snip oversized tool-result contents in the kept window.

        Returns ``(messages, snipped_count, saved_chars)``. Copy-on-write:
        snipped messages are replaced with shallow copies so the original
        dicts — which the host flushes into the rotated-out session — keep
        their full contents. Everything from the last user message onward
        is spared: a preflight fire inside a tool loop must not snip a
        result the model is still acting on. Head + tail of the content
        survive (the tail keeps closing delimiters such as
        ``</untrusted_tool_result>`` intact).
        """
        limit = self.snip_tool_chars
        if limit < 200:  # 0 (or an absurdly small value) disables snipping
            return kept, 0, 0
        last_user = None
        for i in range(len(kept) - 1, -1, -1):
            if kept[i].get("role") == "user":
                last_user = i
                break
        if last_user is None:
            return kept, 0, 0
        head_keep = (limit * 3) // 5
        tail_keep = limit // 5
        out = list(kept)
        snipped = 0
        saved = 0
        for i in range(last_user):
            m = out[i]
            if m.get("role") != "tool":
                continue
            content = m.get("content")
            if not isinstance(content, str) or len(content) <= limit:
                continue
            removed = len(content) - head_keep - tail_keep
            replacement = dict(m)
            replacement["content"] = (
                content[:head_keep]
                + f"\n…[mwe-truncate: snipped {removed:,} of {len(content):,} chars]…\n"
                + content[-tail_keep:]
            )
            out[i] = replacement
            snipped += 1
            saved += removed
        return out, snipped, saved


def register(ctx) -> None:
    ctx.register_context_engine(MweTruncateContextEngine())
