from __future__ import annotations

import logging
import threading
import time
import uuid
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any, Optional

if TYPE_CHECKING:
    from .client import ProvizElekto

_logger = logging.getLogger("proviz_elekto.batch")


# ── Exceptions ────────────────────────────────────────────────────────────────

class BatchError(Exception):
    """Raised when a batch request fails (whole-job or per-item)."""


class BatchTimeoutError(BatchError):
    """Raised when waiting for a batch result exceeds the timeout."""


# ── Result types ──────────────────────────────────────────────────────────────

@dataclass
class BatchJobResult:
    """Result of a single request that was processed via the Mistral Batch API."""
    custom_id: str
    body: dict
    prompt_tokens: int
    completion_tokens: int
    actual_cost_usd: Optional[float] = None

    @property
    def total_tokens(self) -> int:
        return self.prompt_tokens + self.completion_tokens

    @property
    def content(self) -> Optional[str]:
        """Convenience accessor: text content of the first choice."""
        try:
            return self.body["choices"][0]["message"]["content"]
        except (KeyError, IndexError, TypeError):
            return None


# ── BatchJob ──────────────────────────────────────────────────────────────────

class BatchJob:
    """Future-like handle for a single request submitted to the batch queue.

    The result is retrieved by polling the proviz server until the Mistral
    batch job completes. Call result() to block until the answer is available.

    Example::

        job = queue.submit([{"role": "user", "content": "Hello"}])
        result = job.result(timeout=300)
        print(result.content)
    """

    def __init__(
        self,
        request_id: str,
        proviz: "ProvizElekto",
        retry_after_ms: int,
    ) -> None:
        self.request_id = request_id
        self._proviz = proviz
        self._retry_after_ms = retry_after_ms

    def result(self, timeout: float = 3600.0) -> BatchJobResult:
        """Block until the batch result is available and return it.

        Args:
            timeout: Maximum seconds to wait. Default 3600 (1 hour).

        Raises:
            BatchError: If the request failed.
            BatchTimeoutError: If timeout elapses before a result appears.
        """
        deadline = time.monotonic() + timeout

        # Initial sleep: use server's hint so we don't hammer the endpoint.
        initial_sleep = min(self._retry_after_ms / 1000.0, max(0.0, deadline - time.monotonic()))
        if initial_sleep > 0:
            time.sleep(initial_sleep)

        poll_interval = 10.0
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise BatchTimeoutError(
                    f"timed out waiting for batch result {self.request_id!r}"
                )

            try:
                resp = self._proviz._get(f"/batch/result/{self.request_id}")
            except Exception as exc:
                _logger.warning("batch result poll error: %s", exc)
                time.sleep(min(poll_interval, remaining))
                continue

            status = resp.get("status")
            if status == "success":
                return BatchJobResult(
                    custom_id=self.request_id,
                    body=resp.get("body", {}),
                    prompt_tokens=resp.get("prompt_tokens", 0),
                    completion_tokens=resp.get("completion_tokens", 0),
                    actual_cost_usd=resp.get("actual_cost_usd"),
                )
            if status == "error":
                raise BatchError(resp.get("message", "unknown batch error"))

            # status == "pending" — sleep and retry
            sleep_hint = resp.get("retry_after_ms", int(poll_interval * 1000))
            sleep_secs = min(sleep_hint / 1000.0, poll_interval, remaining)
            time.sleep(sleep_secs)
            poll_interval = min(poll_interval * 1.5, 60.0)

    def done(self) -> bool:
        """Non-blocking check: True if the result is already available."""
        try:
            resp = self._proviz._get(f"/batch/result/{self.request_id}")
            return resp.get("status") in ("success", "error")
        except Exception:
            return False


# ── BatchQueue ────────────────────────────────────────────────────────────────

class BatchQueue:
    """Submit LLM requests to the server-side batch queue for Mistral Batch API processing.

    Each ``submit()`` call is forwarded immediately to the proviz server, which
    accumulates requests from all workers and flushes them to Mistral's Batch API
    after ``window_secs`` (configured on the server). Multiple processes/workers
    share the same server-side queue automatically.

    The 50% cost discount applies to all requests routed through this queue.
    Only Mistral text/code models support batch; the server enforces this.

    Args:
        proviz: An active ``ProvizElekto`` client connected to the proviz server.
        step: Step name used for model selection (same as in ``select()``).
        **select_kwargs: Extra keyword arguments forwarded to the server's
            ``/batch/submit`` endpoint (e.g. ``quality_min``, ``group_name``,
            ``requires_fn_call``).

    Example::

        queue = pz.create_batch_queue("classify")
        jobs = [queue.submit([{"role": "user", "content": t}]) for t in texts]
        results = [j.result(timeout=300) for j in jobs]
    """

    def __init__(
        self,
        proviz: "ProvizElekto",
        step: str,
        **select_kwargs: Any,
    ) -> None:
        self._proviz = proviz
        self._step = step
        self._select_kwargs = select_kwargs

    def submit(
        self,
        messages: list[dict],
        **extra_body: Any,
    ) -> BatchJob:
        """Enqueue a chat completion request and return a BatchJob future.

        Args:
            messages: List of message dicts (same format as LiteLLM/OpenAI).
            **extra_body: Optional fields forwarded into the Mistral request body,
                e.g. ``max_tokens=256``, ``temperature=0.7``.

        Returns:
            A ``BatchJob`` whose ``result()`` method blocks until the answer arrives.
        """
        from .client import _estimate_tokens

        payload: dict = {
            "step": self._step,
            "estimated_tokens": _estimate_tokens(messages),
            "messages": messages,
            **self._select_kwargs,
        }
        if extra_body:
            payload["extra_body"] = extra_body

        resp = self._proviz._post("/batch/submit", payload)
        return BatchJob(
            request_id=resp["request_id"],
            proviz=self._proviz,
            retry_after_ms=resp.get("retry_after_ms", 60_000),
        )
