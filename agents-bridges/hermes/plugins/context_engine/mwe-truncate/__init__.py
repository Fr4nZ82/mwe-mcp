"""mwe-truncate context engine for hermes-agent — the mwe-mcp bridge, context half.

The sessionless model of the mwe-mcp per-turn contract (v1): the bounded
recent window is the only conversational state hermes keeps; long-range
continuity comes from the per-turn recall block (`memory.provider: mwe`),
not from a compression summary. `compress()` therefore **truncates** — it
keeps the system messages, a small protected head, and the last N
messages, and drops the middle with **no summarization pass** (no LLM
call, no summary message).

Config (optional), in `config.yaml`:

    context:
      engine: mwe-truncate
      mwe-truncate:
        threshold_percent: 0.75   # fire when prompt tokens exceed this share of context
        protect_first_n: 3        # non-system messages preserved at the head
        protect_last_n: 16        # non-system messages preserved at the tail
"""

from __future__ import annotations

import logging
from typing import Any, Dict, List

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
    """Truncate to a bounded window; recall replaces the summary."""

    def __init__(self):
        self.threshold_percent = float(_cfg("threshold_percent", 0.75))
        self.protect_first_n = int(_cfg("protect_first_n", 3))
        self.protect_last_n = int(_cfg("protect_last_n", 16))
        self.last_prompt_tokens = 0
        self.last_completion_tokens = 0
        self.last_total_tokens = 0
        self.threshold_tokens = 0
        self.context_length = 0
        self.compression_count = 0

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
        self.threshold_tokens = int(self.context_length * self.threshold_percent)

    # -- compaction ------------------------------------------------------------

    def should_compress(self, prompt_tokens: int = None) -> bool:
        tokens = self.last_prompt_tokens if prompt_tokens is None else prompt_tokens
        return self.threshold_tokens > 0 and tokens >= self.threshold_tokens

    def has_content_to_compress(self, messages: List[Dict[str, Any]]) -> bool:
        return len(self._droppable_indices(messages)) > 0

    def compress(self, messages: List[Dict[str, Any]], current_tokens: int = None,
                 focus_topic: str = None) -> List[Dict[str, Any]]:
        # focus_topic is a summarization-engine concept; truncation keeps the
        # same window regardless, and recall surfaces the topic when relevant.
        drop = self._droppable_indices(messages)
        if not drop:
            return messages
        kept = [m for i, m in enumerate(messages) if i not in drop]
        self.compression_count += 1
        logger.info("mwe-truncate: dropped %d of %d messages (window: first %d + last %d)",
                    len(drop), len(messages), self.protect_first_n, self.protect_last_n)
        return kept

    def get_status(self) -> Dict[str, Any]:
        return {
            "engine": self.name,
            "last_prompt_tokens": self.last_prompt_tokens,
            "threshold_tokens": self.threshold_tokens,
            "context_length": self.context_length,
            "compression_count": self.compression_count,
            "protect_first_n": self.protect_first_n,
            "protect_last_n": self.protect_last_n,
        }

    # -- internals ---------------------------------------------------------

    def _droppable_indices(self, messages: List[Dict[str, Any]]) -> set:
        """Indices of the middle slice, with tool-pairing-safe boundaries.

        Non-system messages keep the first `protect_first_n` and the last
        `protect_last_n`; everything between is dropped. Both boundaries
        are then walked so no kept assistant tool-call loses its tool
        result and no kept tool result loses its call: the head shrinks
        until it ends on a user message or a plain assistant message, the
        tail grows backwards until it starts on a user message.
        """
        non_system = [i for i, m in enumerate(messages) if m.get("role") != "system"]
        head = non_system[:self.protect_first_n]
        tail = non_system[max(self.protect_first_n,
                              len(non_system) - self.protect_last_n):]
        if not tail or not non_system[len(head):-len(tail) or None]:
            return set()

        def is_clean_head_end(idx):
            m = messages[idx]
            return m.get("role") == "user" or (
                m.get("role") == "assistant" and not m.get("tool_calls")
            )

        while head and not is_clean_head_end(head[-1]):
            head.pop()
        while tail and messages[tail[0]].get("role") != "user":
            pos = non_system.index(tail[0])
            if pos == 0 or non_system[pos - 1] in head:
                break
            tail.insert(0, non_system[pos - 1])
        protected = set(head) | set(tail)
        return {i for i in non_system if i not in protected}


def register(ctx) -> None:
    ctx.register_context_engine(MweTruncateContextEngine())
