from __future__ import annotations

import atexit
import json
import logging
import os
import random
import select as _select
import shutil
import socket
import subprocess
import threading
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from typing import Any, Callable, Optional

_logger = logging.getLogger("proviz_elekto")


def _setup_logging() -> None:
    if os.environ.get("LOG_LEVEL", "").upper() in ("DEBUG", "TRACE"):
        _logger.setLevel(logging.DEBUG)
        if not _logger.handlers:
            _h = logging.StreamHandler()
            _h.setFormatter(
                logging.Formatter("%(asctime)s proviz_elekto %(levelname)s %(message)s")
            )
            _logger.addHandler(_h)
    else:
        _logger.addHandler(logging.NullHandler())


_setup_logging()


class ProvizError(Exception):
    pass


class AllModelsExhausted(ProvizError):
    def __init__(self, step: str, tried: int, retry_after_ms: int = 0):
        super().__init__(f"all models exhausted for step '{step}' (tried {tried})")
        self.step = step
        self.tried = tried
        self.retry_after_ms = retry_after_ms


@dataclass
class ModelCandidate:
    model_id: str
    brand_slug: str
    model_slug: str
    api_key_env: Optional[str]
    max_context_tokens: int
    supports_function_calling: bool
    supports_json_mode: bool
    estimated_input_cost_usd: Optional[float]
    # Exact `reasoning_effort` literal /complete will send for this model, or None if it never
    # sends the param. Not a capability flag: acceptance of a value isn't the same as it being
    # effective, so this is the one literal confirmed to actually work for this model.
    reasoning_effort_value: Optional[str] = None
    price_input_per_1m: Optional[float] = None
    price_output_per_1m: Optional[float] = None
    # Per-million price for prompt-cache-hit input tokens (providers that price them separately:
    # DeepSeek, Nous Portal, OpenAI, Anthropic, ...). None when the provider doesn't cache or
    # doesn't quote a distinct rate; _compute_cost then bills every prompt token at the full rate.
    price_cached_input_per_1m: Optional[float] = None
    # Brand's OpenAI-compatible base URL (None for brands using a well-known default endpoint).
    base_url: Optional[str] = None
    # ID of the specific brand API key selected. Echo back in report calls so the server
    # can block the key (not the model) on 429, enabling failover to other keys.
    brand_key_id: Optional[str] = None
    # Multiplier applied to pricing for batch API calls (e.g. 0.5 for 50% discount).
    batch_price_multiplier: Optional[float] = None
    # Shared model-catalog key this row was resolved against, if any (see ModelCatalogEntry
    # server-side). None for models with no known cross-provider identity.
    canonical_key: Optional[str] = None
    # RFC3339 timestamp of when this row's pricing was last synced from a live provider catalog
    # (e.g. OpenRouter). None for hand-curated (non-auto-synced) providers.
    price_synced_at: Optional[str] = None
    # Whether this model/provider may train on submitted data, as reported by the source (e.g.
    # Requesty's data_used_for_training). None when the source doesn't report it.
    trains_on_data: Optional[bool] = None
    # Whether this model/provider retains submitted data. None when not reported.
    retains_data: Optional[bool] = None
    # ISO currency price_input_per_1m / price_output_per_1m are denominated in (echo of the
    # brand's price_currency). "USD" for almost every brand. estimated_input_cost_usd is
    # already in USD regardless. _compute_cost() converts non-USD figures via /fx/rates.
    price_currency: str = "USD"


@dataclass
class CallResult:
    response: Any
    candidate: ModelCandidate
    provider: str
    prompt_tokens: int
    completion_tokens: int
    total_tokens: int
    actual_cost_usd: Optional[float] = None
    # Of prompt_tokens, how many were prompt-cache hits (billed at the discounted cached rate).
    cached_tokens: int = 0


@dataclass
class CompleteResult:
    """Result of a server-side /complete call (select + provider call + report in one round-trip)."""
    text: str
    model: str
    brand: str
    prompt_tokens: int
    completion_tokens: int
    cost_usd: Optional[float] = None
    # Un-executed tool calls returned by the provider (caller drives the tool loop).
    tool_calls: Optional[Any] = None
    # Of prompt_tokens, how many were prompt-cache hits (billed at the discounted cached rate).
    # 0 when the provider doesn't cache or the prefix wasn't warm. Lets a caller tell whether its
    # prompt prefix is actually being cached across calls.
    cached_tokens: int = 0


def _classify_error(exc: Exception) -> tuple[str, str]:
    status = getattr(exc, "status_code", None)
    cls = type(exc).__name__
    cls_lower = cls.lower()
    msg = str(exc).lower()

    is_rate_limit = (
        status == 429
        or "ratelimit" in cls_lower
        or "rate_limit" in cls_lower
        # some providers surface 429 via response body without setting status_code
        or ("429" in msg and "rate" in msg)
    )

    if is_rate_limit:
        if "day" in msg or "tpd" in msg or "daily" in msg:
            return "rate_limit", "tpd"
        if "token" in msg or "tpm" in msg:
            return "rate_limit", "tpm"
        return "rate_limit", "rpm"

    if status in (401, 403) or "auth" in cls_lower:
        return "error", "auth"

    if "timeout" in cls_lower or status == 408:
        return "error", "timeout"

    return "error", "other"


def _extract_usage(response: Any) -> tuple[int, int, int]:
    try:
        usage = response.usage
        prompt = getattr(usage, "prompt_tokens", 0) or 0
        completion = getattr(usage, "completion_tokens", 0) or 0
        return prompt, completion, prompt + completion
    except AttributeError:
        return 0, 0, 0


def _collect_response_headers(response: Any) -> dict[str, str]:
    """Merge HTTP response headers from all locations a LiteLLM response may use.

    LiteLLM stores headers under different keys depending on provider and version,
    and sometimes as plain dicts, sometimes as httpx.Headers or similar objects.
    We scan every known location and merge into one lowercase-keyed dict so callers
    don't need to know which provider/version combination is in use.
    """
    merged: dict[str, str] = {}

    def _absorb(obj: Any) -> None:
        if obj is None:
            return
        try:
            items = obj.items() if hasattr(obj, "items") else obj
            for k, v in items:
                if isinstance(k, str):
                    # LiteLLM prefixes provider headers with "llm_provider-"; strip it.
                    k = k.lower().removeprefix("llm_provider-")
                    if k not in merged:
                        merged[k] = str(v)
        except Exception:
            pass

    # Direct attributes on the response object (some LiteLLM versions / providers)
    for attr in ("headers", "_headers", "_response_headers", "response_headers"):
        _absorb(getattr(response, attr, None))

    # _hidden_params — all known keys across LiteLLM versions
    hidden = getattr(response, "_hidden_params", None)
    if isinstance(hidden, dict):
        for key in (
            "additional_headers",
            "response_headers",
            "headers",
            "_response_headers",
            "litellm_response_headers",
        ):
            _absorb(hidden.get(key))

        # Raw HTTP response object stored under various keys on some LiteLLM code
        # paths (e.g. tool-use completions, newer versions).  Pull headers directly
        # from the underlying httpx.Response if present.
        for key in ("response", "httpx_response", "_response", "raw_response"):
            raw = hidden.get(key)
            if raw is not None:
                for attr in ("headers", "_headers", "response_headers"):
                    _absorb(getattr(raw, attr, None))

        # Broad fallback: any remaining _hidden_params value that looks like a
        # header container but wasn't already scanned above.  Catches future or
        # unknown LiteLLM key names without requiring code changes here.
        _known_keys = frozenset((
            "additional_headers", "response_headers", "headers", "_response_headers",
            "litellm_response_headers", "response", "httpx_response", "_response",
            "raw_response",
        ))
        for key, val in hidden.items():
            if key in _known_keys or val is None or not hasattr(val, "items"):
                continue
            _absorb(val)

    if _logger.isEnabledFor(logging.DEBUG) and not merged:
        _logger.debug(
            "_collect_response_headers: no headers found — "
            "response type=%s, direct attrs=%s, hidden_params keys=%s",
            type(response).__name__,
            {a: type(getattr(response, a, None)).__name__
             for a in ("headers", "_headers", "_response_headers", "response_headers")},
            list(hidden.keys()) if isinstance(hidden, dict) else repr(hidden),
        )

    return merged


def _extract_provider_limits(
    response: Any,
) -> tuple[Optional[int], Optional[int], Optional[int], Optional[int]]:
    """Extract remaining and limit request/token values from provider response headers.

    Returns ``(remaining_requests, remaining_tokens, limit_requests, limit_tokens)``.

    Handles OpenAI/Mistral/Groq style headers (`x-ratelimit-remaining-*`, `x-ratelimit-limit-*`)
    and Anthropic style (`anthropic-ratelimit-*-remaining`, `anthropic-ratelimit-*-limit`).
    Scans all locations where LiteLLM may store headers across providers and versions.
    """
    headers = _collect_response_headers(response)
    if not headers:
        return None, None, None, None

    def _parse(keys: tuple[str, ...]) -> Optional[int]:
        for key in keys:
            val = headers.get(key)
            if val is not None:
                try:
                    return int(val)
                except (ValueError, TypeError):
                    pass
        return None

    remaining_requests = _parse((
        "x-ratelimit-remaining-requests",
        "ratelimit-remaining-requests",
        "x-ratelimit-remaining-req-minute",
        "anthropic-ratelimit-requests-remaining",
    ))
    remaining_tokens = _parse((
        "x-ratelimit-remaining-tokens",
        "ratelimit-remaining-tokens",
        "x-ratelimit-remaining-tokens-minute",
        "anthropic-ratelimit-tokens-remaining",
    ))
    limit_requests = _parse((
        "x-ratelimit-limit-requests",
        "ratelimit-limit-requests",
        "x-ratelimit-limit-req-minute",
        "anthropic-ratelimit-requests-limit",
    ))
    limit_tokens = _parse((
        "x-ratelimit-limit-tokens",
        "ratelimit-limit-tokens",
        "x-ratelimit-limit-tokens-minute",
        "anthropic-ratelimit-tokens-limit",
    ))

    if remaining_requests is None and remaining_tokens is None and _logger.isEnabledFor(logging.DEBUG):
        _logger.debug(
            "_extract_provider_limits: no remaining_requests/tokens — "
            "available header keys: %s",
            sorted(headers.keys()) or "none",
        )

    return remaining_requests, remaining_tokens, limit_requests, limit_tokens


def _extract_remaining_limits(response: Any) -> tuple[Optional[int], Optional[int]]:
    """Thin wrapper kept for backwards compatibility. Prefer _extract_provider_limits."""
    rem_req, rem_tok, _, _ = _extract_provider_limits(response)
    return rem_req, rem_tok


def _extract_provider_cost(response: Any) -> Optional[float]:
    """Extract the real, request-specific cost the provider itself reported (e.g. OpenRouter's
    `usage.cost`, computed server-side from whichever upstream sub-provider actually served the
    request). Distinct from LiteLLM's own `_hidden_params["response_cost"]`, which is LiteLLM's
    internally *computed* estimate from its own pricing table — never used here, since it isn't
    the provider's real figure and would misrepresent what was actually charged.

    Returns None when the provider doesn't return one (most providers have a single fixed price,
    so `_compute_cost` from the catalog price is already exact for them).
    """
    usage = getattr(response, "usage", None)
    if usage is None:
        return None

    def _as_float(v: Any) -> Optional[float]:
        try:
            return float(v)
        except (TypeError, ValueError):
            return None

    # `usage.cost` — OpenRouter. `usage.cost_usd` — OrcaRouter (returned only when the
    # `X-OrcaRouter-Include-Cost: true` request header is sent; see call_litellm below).
    cost = _as_float(getattr(usage, "cost", None))
    if cost is None:
        cost = _as_float(getattr(usage, "cost_usd", None))
    if cost is not None:
        return cost

    # Pydantic v2 stores fields a model doesn't declare in `model_extra` when the model allows
    # extras; some LiteLLM versions surface OpenRouter's `usage.cost` there instead of as a plain
    # attribute.
    extra = getattr(usage, "model_extra", None)
    if isinstance(extra, dict):
        cost = _as_float(extra.get("cost")) or _as_float(extra.get("cost_usd"))
        if cost is not None:
            return cost
        details = extra.get("cost_details")
        if isinstance(details, dict):
            cost = _as_float(details.get("upstream_inference_cost"))
            if cost is not None:
                return cost

    return None


def _extract_cached_tokens(response: Any) -> int:
    """Prompt-cache hit count from the provider response usage block. Tries the OpenAI/Nous shape
    (`usage.prompt_tokens_details.cached_tokens`) then DeepSeek's first-party field
    (`usage.prompt_cache_hit_tokens`). 0 when neither is present."""
    usage = getattr(response, "usage", None)
    if usage is None:
        return 0

    def _as_int(v: Any) -> int:
        try:
            return int(v)
        except (TypeError, ValueError):
            return 0

    details = getattr(usage, "prompt_tokens_details", None)
    if details is not None:
        n = _as_int(getattr(details, "cached_tokens", None))
        if n:
            return n
        if isinstance(details, dict):
            n = _as_int(details.get("cached_tokens"))
            if n:
                return n
    n = _as_int(getattr(usage, "prompt_cache_hit_tokens", None))
    if n:
        return n
    extra = getattr(usage, "model_extra", None)
    if isinstance(extra, dict):
        pd = extra.get("prompt_tokens_details")
        if isinstance(pd, dict):
            n = _as_int(pd.get("cached_tokens"))
            if n:
                return n
        n = _as_int(extra.get("prompt_cache_hit_tokens"))
        if n:
            return n
    return 0


def _compute_cost(
    candidate: "ModelCandidate",
    prompt_tokens: int,
    completion_tokens: int,
    fx_rates: Optional[dict] = None,
    cached_tokens: int = 0,
) -> Optional[float]:
    """Compute actual cost in USD from model prices and token counts. Returns None if prices
    unknown. `candidate.price_currency` may be non-USD (see ModelCandidate); `fx_rates` maps
    currency -> units-per-USD (from the server's /fx/rates). Missing rate -> treated as USD.

    `cached_tokens` (of `prompt_tokens`) are billed at `candidate.price_cached_input_per_1m` when
    the provider quotes one; otherwise they fall back to the full input rate, so passing a count
    for a provider with no cached price is a no-op."""
    p_in = candidate.price_input_per_1m
    p_out = candidate.price_output_per_1m
    if p_in is None and p_out is None:
        return None
    cur = (candidate.price_currency or "USD").upper()
    rate = 1.0
    if cur != "USD" and fx_rates:
        r = fx_rates.get(cur)
        if r and r > 0:
            rate = r
    full_in = p_in or 0.0
    cached = max(0, min(int(cached_tokens or 0), prompt_tokens))
    uncached = prompt_tokens - cached
    cached_rate = candidate.price_cached_input_per_1m
    if cached_rate is None:
        cached_rate = full_in
    native = (
        full_in * uncached + cached_rate * cached + (p_out or 0.0) * completion_tokens
    ) / 1_000_000.0
    return native / rate


def _estimate_tokens(messages: list[dict]) -> int:
    total = sum(len(str(m.get("content", ""))) for m in messages)
    return max(1, total // 4)


# Brands that are OpenAI-compatible but not registered as first-class litellm providers.
# They must be driven through litellm's "openai/" pseudo-provider with an explicit api_base.
# Infomaniak is one: its base_url embeds an account-specific product_id that the proviz server
# resolves (from ${INFOMANIAK_PRODUCT_ID}) into candidate.base_url. Split-flow callers using
# call_litellm* get the resolved URL echoed back on the candidate; the dependency-free
# complete() path needs none of this since the server makes the call itself.
#
# OrcaRouter is OpenAI-compatible (base_url https://api.orcarouter.ai/v1) but litellm has no
# first-class provider for it, so it's routed as openai/<slug> with an explicit api_base too.
# TokenHub (base_url https://us-api.tokenhub.com/v1) is the same situation — plain
# OpenAI-compatible, no first-class litellm provider, no special headers/params needed (its
# real per-request cost comes back as usage.cost, already handled by _extract_provider_cost).
_OPENAI_COMPAT_BRANDS = {"infomaniak", "orcarouter", "tokenhub"}


def _litellm_target(candidate: "ModelCandidate") -> tuple[str, dict]:
    """Return (model_id, extra_kwargs) for litellm.completion() for this candidate.

    For OrcaRouter the extras also mirror what the server's /complete path does for that brand:
    the `X-OrcaRouter-Include-Cost` header (so `usage.cost_usd` comes back — see
    _extract_provider_cost) and a `reasoning_effort="minimal"` default (OrcaRouter has no
    reasoning-off switch; a caller-supplied reasoning_effort still overrides this downstream).
    """
    prefix = (candidate.brand_slug or "").split("-")[0]
    if prefix in _OPENAI_COMPAT_BRANDS:
        extra: dict = {}
        if candidate.base_url:
            extra["api_base"] = candidate.base_url
        if prefix == "orcarouter":
            extra["extra_headers"] = {"X-OrcaRouter-Include-Cost": "true"}
            extra["reasoning_effort"] = "minimal"
        return f"openai/{candidate.model_slug}", extra
    return f"{candidate.brand_slug}/{candidate.model_slug}", {}


def _find_binary() -> str:
    """Locate the proviz-server binary bundled with the package or on PATH."""
    import sysconfig

    pkg_dir = os.path.dirname(__file__)
    for name in ("proviz-server", "proviz-server.exe"):
        bundled = os.path.join(pkg_dir, name)
        if os.path.isfile(bundled) and os.access(bundled, os.X_OK):
            return bundled

    # maturin bindings="bin" installs the binary to the env's scripts directory
    scripts_dir = sysconfig.get_path("scripts")
    if scripts_dir:
        for name in ("proviz-server", "proviz-server.exe"):
            candidate = os.path.join(scripts_dir, name)
            if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
                return candidate

    on_path = shutil.which("proviz-server")
    if on_path:
        return on_path

    # Dev fallback: walk up from the package dir looking for a Cargo workspace build output
    check = os.path.dirname(__file__)
    for _ in range(6):
        for rel in ("target/release/proviz-server", "target/debug/proviz-server"):
            candidate = os.path.join(check, rel)
            if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
                return candidate
        check = os.path.dirname(check)

    raise ProvizError(
        "proviz-server binary not found. "
        "Reinstall the package or build from source: cargo build --release --bin proviz-server"
    )


class ProvizElekto:
    """
    Smart LLM model router.

    Automatically starts the proviz-server binary as a subprocess on first use,
    unless `host` is set to a non-localhost value (e.g. a Docker service name) — in
    that case it attaches to the already-running remote server without spawning.

    Usage:
        pz = ProvizElekto(database_url="postgresql://...")
        candidate = pz.select(step="verdict", estimated_tokens=2500)
        ...
        pz.report_success(candidate.model_id)

    Docker / remote usage:
        pz = ProvizElekto(host="proviz", port=63130)
        # or: PROVIZ_HOST=proviz PROVIZ_PORT=63130 in environment
    """

    def __init__(
        self,
        database_url: Optional[str] = None,
        db_path: str = "proviz.db",
        host: str = "localhost",
        port: int = 0,
        timeout: float = 5.0,
        startup_timeout: float = 10.0,
        sync_provider_limits: bool = False,
    ):
        self._proc: Optional[subprocess.Popen] = None
        host = os.environ.get("PROVIZ_HOST", host)
        port = int(os.environ.get("PROVIZ_PORT", port))
        self._host = host
        self._port = port
        self._base = f"http://{host}:{port}"
        self._timeout = timeout
        self._sync_provider_limits = sync_provider_limits
        self._fx_cache: Optional[dict] = None
        self._fx_cache_at: float = 0.0

        # If pointing at a remote host (not localhost), never spawn — just attach.
        if host != "localhost" and port > 0:
            return

        if port > 0 and self._is_running():
            return

        self._start(
            database_url=database_url,
            db_path=db_path,
            startup_timeout=startup_timeout,
        )
        atexit.register(self._stop)

    # ── Lifecycle ─────────────────────────────────────────────────────────────

    def _is_running(self) -> bool:
        try:
            with socket.create_connection((self._host, self._port), timeout=0.5):
                return True
        except OSError:
            return False

    def _start(
        self,
        database_url: Optional[str],
        db_path: str,
        startup_timeout: float,
    ) -> None:
        binary = _find_binary()
        args = [binary, "--port", str(self._port)]
        if database_url:
            args += ["--storage", "postgres", "--database-url", database_url]
        else:
            args += ["--storage", "sqlite", "--db-path", db_path]

        self._proc = subprocess.Popen(
            args,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )

        deadline = time.monotonic() + startup_timeout
        line = b""
        while time.monotonic() < deadline:
            if self._proc.poll() is not None:
                err = self._proc.stderr.read().decode(errors="replace")
                raise ProvizError(f"proviz-server exited early:\n{err}")
            ready, _, _ = _select.select([self._proc.stdout], [], [], 0.05)
            if ready:
                line = self._proc.stdout.readline()
                break
        else:
            self._proc.kill()
            raise ProvizError(f"proviz-server did not print port within {startup_timeout}s")

        text = line.decode().strip()
        if not text.startswith("PROVIZ_PORT="):
            self._proc.kill()
            raise ProvizError(f"proviz-server unexpected stdout: {text!r}")

        actual_port = int(text.split("=", 1)[1])
        self._port = actual_port
        self._base = f"http://{self._host}:{actual_port}"

    def _stop(self) -> None:
        if self._proc and self._proc.poll() is None:
            self._proc.terminate()
            try:
                self._proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self._proc.kill()

    # ── HTTP helpers ──────────────────────────────────────────────────────────

    # 5xx / connection-level failures get a short bounded retry — distinct from
    # AllModelsExhausted's own backoff-and-retry (that one is expected/routine heatroom
    # backpressure, this one is an unexpected server-side fault). Motivating case: proviz-server's
    # storage layer used to hold a single postgres connection with no reconnect, so ANY transient
    # DB blip (crash-restart, network hiccup) permanently 500'd every /select and /complete call
    # for every caller until the whole proviz process was restarted by hand. The storage layer now
    # self-heals on its NEXT query (see proviz-elekto server/storage-pg `connected_client()`), but
    # the one request that lands in the split-second before that self-heal still needs to be
    # retried once to actually benefit from it — otherwise a caller like the detector just drops
    # that claim-detection cycle on the first failure and moves on, no crash but silent data loss.
    # 4xx is a caller mistake (bad payload, bad step name) — retrying it can't help, fail fast.
    _TRANSIENT_RETRY_DELAY_S = 0.3

    def _request_with_retry(
        self, path: str, *, data: Optional[bytes] = None, method: str = "GET",
        max_retries: int = 2,
    ) -> dict:
        headers = {"Content-Type": "application/json"} if data is not None else {}
        last_exc: Optional[Exception] = None
        for attempt in range(max_retries + 1):
            req = urllib.request.Request(
                f"{self._base}{path}", data=data, headers=headers, method=method,
            )
            try:
                with urllib.request.urlopen(req, timeout=self._timeout) as resp:
                    return json.loads(resp.read())
            except urllib.error.HTTPError as e:
                payload = json.loads(e.read())
                if payload.get("error") == "all_models_exhausted":
                    raise AllModelsExhausted(
                        step=payload.get("step", "?"),
                        tried=payload.get("tried", 0),
                        retry_after_ms=payload.get("retry_after_ms", 0),
                    )
                last_exc = ProvizError(f"HTTP {e.code}: {payload}")
                if e.code < 500 or attempt == max_retries:
                    raise last_exc from e
                _logger.warning(
                    "ProvizElekto %s HTTP %d (attempt %d/%d), retrying: %s",
                    path, e.code, attempt + 1, max_retries + 1, payload,
                )
            except (urllib.error.URLError, ConnectionError, TimeoutError, OSError) as e:
                last_exc = ProvizError(str(e))
                if attempt == max_retries:
                    raise last_exc from e
                _logger.warning(
                    "ProvizElekto %s connection error (attempt %d/%d), retrying: %s",
                    path, attempt + 1, max_retries + 1, e,
                )
            time.sleep(self._TRANSIENT_RETRY_DELAY_S * (attempt + 1))
        raise last_exc  # pragma: no cover — loop above always returns or raises

    def _post(self, path: str, body: dict) -> dict:
        data = json.dumps(body).encode()
        return self._request_with_retry(path, data=data, method="POST")

    def _get(self, path: str) -> dict:
        return self._request_with_retry(path, method="GET")

    def _fx_rates(self) -> dict:
        """currency -> units-per-USD, from the server's /fx/rates. Cached in-process for 1 h.
        Returns {} on any failure (so _compute_cost falls back to treating prices as USD)."""
        now = time.monotonic()
        if self._fx_cache is not None and (now - self._fx_cache_at) < 3600:
            return self._fx_cache
        try:
            rates = self._get("/fx/rates").get("rates", {}) or {}
            rates = {str(k).upper(): float(v) for k, v in rates.items()}
        except Exception:  # noqa: BLE001 — cost is best-effort
            rates = {}
        self._fx_cache = rates
        self._fx_cache_at = now
        return rates

    def _post_fire_and_forget(self, path: str, body: dict) -> None:
        """Send a report in a background thread — caller is not blocked.

        Used exclusively for success reports where the LLM response is already
        in hand. Rate-limit and error reports must use _post (synchronous) so
        that the model is blocked in proviz before the retry select() call.
        """
        def _send() -> None:
            try:
                self._post(path, body)
            except Exception as exc:
                _logger.debug("background report failed (path=%s): %s", path, exc)

        threading.Thread(target=_send, daemon=True, name="proviz-report").start()

    # ── Public API ────────────────────────────────────────────────────────────

    def select(
        self,
        step: str,
        estimated_tokens: int,
        requires_fn_call: bool = False,
        requires_json_mode: bool = False,
        requires_streaming: Optional[bool] = None,
        quality_min: float = 0.0,
        exclude_ids: Optional[list[str]] = None,
        categories: Optional[list[str]] = None,
        languages: Optional[list[str]] = None,
        group_id: Optional[str] = None,
        group_name: Optional[str] = None,
        use_member_priority: bool = True,
        max_wait_ms: Optional[int] = None,
        require_no_training: bool = False,
        cost_weight: Optional[float] = None,
        latency_weight: Optional[float] = None,
        quality_weight: Optional[float] = None,
    ) -> ModelCandidate:
        """
        require_no_training: only select models the source explicitly confirms don't train on
        submitted data (unknown status is excluded too, not treated as safe — most providers,
        including OpenRouter, don't report this at all). See ModelCandidate.trains_on_data.

        cost_weight/latency_weight/quality_weight: override the Pass-2 scoring weight for that
        component (built-in defaults 0.15/0.10/0.20). Omitted → the group's own
        cost_weight_override/latency_weight_override/quality_weight_override (when group-based)
        or the built-in default. Setting one renormalizes the whole weight set to sum to 1.0, so
        raising it shrinks the others proportionally rather than zeroing them out.
        """
        payload: dict = {
            "step": step,
            "estimated_tokens": estimated_tokens,
            "requires_fn_call": requires_fn_call,
            "requires_json_mode": requires_json_mode,
            "quality_min": quality_min,
            "exclude_ids": exclude_ids or [],
            "categories": categories or [],
            "languages": languages or [],
            "use_member_priority": use_member_priority,
            "require_no_training": require_no_training,
        }
        if requires_streaming is not None:
            payload["requires_streaming"] = requires_streaming
        if group_id is not None:
            payload["group_id"] = group_id
        if group_name is not None:
            payload["group_name"] = group_name
        if max_wait_ms is not None:
            payload["max_wait_ms"] = max_wait_ms
        if cost_weight is not None:
            payload["cost_weight"] = cost_weight
        if latency_weight is not None:
            payload["latency_weight"] = latency_weight
        if quality_weight is not None:
            payload["quality_weight"] = quality_weight
        _logger.debug(
            "select request: step=%s estimated_tokens=%d group_name=%s group_id=%s max_wait_ms=%s",
            step, estimated_tokens, group_name, group_id, max_wait_ms,
        )
        r = self._post("/select", payload)
        candidate = ModelCandidate(
            model_id=r["model_id"],
            brand_slug=r["brand_slug"],
            model_slug=r["model_slug"],
            api_key_env=r.get("api_key_env"),
            max_context_tokens=r["max_context_tokens"],
            supports_function_calling=r["supports_function_calling"],
            supports_json_mode=r["supports_json_mode"],
            estimated_input_cost_usd=r.get("estimated_input_cost_usd"),
            price_input_per_1m=r.get("price_input_per_1m"),
            price_output_per_1m=r.get("price_output_per_1m"),
            price_cached_input_per_1m=r.get("price_cached_input_per_1m"),
            base_url=r.get("base_url"),
            brand_key_id=r.get("brand_key_id"),
            reasoning_effort_value=r.get("reasoning_effort_value"),
            canonical_key=r.get("canonical_key"),
            price_synced_at=r.get("price_synced_at"),
            trains_on_data=r.get("trains_on_data"),
            retains_data=r.get("retains_data"),
            price_currency=r.get("price_currency", "USD"),
        )
        _logger.debug(
            "select response: model=%s brand=%s cost_usd=%s",
            candidate.model_slug, candidate.brand_slug, candidate.estimated_input_cost_usd,
        )
        return candidate

    def complete(
        self,
        step: str,
        messages: list[dict],
        *,
        estimated_tokens: Optional[int] = None,
        requires_fn_call: bool = False,
        requires_json_mode: bool = False,
        quality_min: float = 0.0,
        exclude_ids: Optional[list[str]] = None,
        categories: Optional[list[str]] = None,
        languages: Optional[list[str]] = None,
        group_id: Optional[str] = None,
        group_name: Optional[str] = None,
        max_wait_ms: Optional[int] = None,
        temperature: Optional[float] = None,
        max_tokens: Optional[int] = None,
        response_format: Optional[dict] = None,
        tools: Optional[list[dict]] = None,
        tool_choice: Optional[Any] = None,
        timeout_secs: Optional[int] = None,
        require_no_training: bool = False,
        cost_weight: Optional[float] = None,
        latency_weight: Optional[float] = None,
        quality_weight: Optional[float] = None,
    ) -> CompleteResult:
        """Server-side select + provider call + report in a single round-trip.

        Unlike `call_litellm`, the LLM call happens **inside proviz-server** — the caller needs no
        litellm or provider SDK. The server picks a model, POSTs to the provider's OpenAI-compatible
        `/chat/completions`, reports usage back to the selector, and returns the parsed result.

        `tools`/`tool_choice` are forwarded to the provider; returned `tool_calls` are NOT executed —
        the caller drives the tool loop and re-submits.

        There's no `reasoning_effort` param here: the server injects the right literal itself from
        `ModelCandidate.reasoning_effort_value` of whichever model it picks (or omits the param
        entirely when that's `None`). The caller can't supply this — selection happens atomically
        inside this same round-trip, so only the server knows which model was chosen, and acceptance
        of a value isn't the same as it being effective (a model can 200 on any literal while only
        one actually changes its behavior).

        require_no_training: only select models the source explicitly confirms don't train on
        submitted data (see ModelCandidate.trains_on_data). The server also sends OpenRouter's
        request-level opt-out (`provider: {data_collection: "deny", zdr: true}`) whenever this
        is set, regardless of which brand gets selected — a harmless no-op for providers that
        don't recognize the field.
        """
        if estimated_tokens is None:
            estimated_tokens = _estimate_tokens(messages)
        payload: dict = {
            "step": step,
            "estimated_tokens": estimated_tokens,
            "requires_fn_call": requires_fn_call,
            "requires_json_mode": requires_json_mode,
            "quality_min": quality_min,
            "exclude_ids": exclude_ids or [],
            "categories": categories or [],
            "languages": languages or [],
            "messages": messages,
            "require_no_training": require_no_training,
        }
        if group_id is not None:
            payload["group_id"] = group_id
        if group_name is not None:
            payload["group_name"] = group_name
        if max_wait_ms is not None:
            payload["max_wait_ms"] = max_wait_ms
        if temperature is not None:
            payload["temperature"] = temperature
        if max_tokens is not None:
            payload["max_tokens"] = max_tokens
        if response_format is not None:
            payload["response_format"] = response_format
        if tools is not None:
            payload["tools"] = tools
        if tool_choice is not None:
            payload["tool_choice"] = tool_choice
        if timeout_secs is not None:
            payload["timeout_secs"] = timeout_secs
        if cost_weight is not None:
            payload["cost_weight"] = cost_weight
        if latency_weight is not None:
            payload["latency_weight"] = latency_weight
        if quality_weight is not None:
            payload["quality_weight"] = quality_weight

        # /complete may block server-side for a provider call; relax the client read timeout.
        prev_timeout = self._timeout
        if timeout_secs is not None:
            self._timeout = max(self._timeout, float(timeout_secs) + 5.0)
        try:
            r = self._post("/complete", payload)
        finally:
            self._timeout = prev_timeout

        result = CompleteResult(
            text=r.get("text", ""),
            model=r.get("model", ""),
            brand=r.get("brand", ""),
            prompt_tokens=r.get("prompt_tokens", 0),
            completion_tokens=r.get("completion_tokens", 0),
            cost_usd=r.get("cost_usd"),
            tool_calls=r.get("tool_calls"),
            cached_tokens=r.get("cached_tokens", 0) or 0,
        )
        _logger.debug(
            "complete: model=%s/%s prompt=%d completion=%d cost_usd=%s",
            result.brand, result.model, result.prompt_tokens, result.completion_tokens,
            result.cost_usd,
        )
        return result

    def report_success(
        self,
        model_id: str,
        estimated_tokens: int = 0,
        actual_tokens: Optional[int] = None,
        prompt_tokens: Optional[int] = None,
        completion_tokens: Optional[int] = None,
        cached_tokens: Optional[int] = None,
        remaining_requests: Optional[int] = None,
        remaining_tokens: Optional[int] = None,
        limit_requests: Optional[int] = None,
        limit_tokens: Optional[int] = None,
        brand_key_id: Optional[str] = None,
        actual_cost_usd: Optional[float] = None,
        response_time_ms: Optional[int] = None,
    ) -> None:
        _logger.debug(
            "report: model_id=%s outcome=success prompt=%s completion=%s remaining_req=%s "
            "remaining_tok=%s limit_req=%s limit_tok=%s sync=%s actual_cost_usd=%s "
            "response_time_ms=%s",
            model_id, prompt_tokens, completion_tokens, remaining_requests, remaining_tokens,
            limit_requests, limit_tokens, self._sync_provider_limits, actual_cost_usd,
            response_time_ms,
        )
        payload: dict = {"model_id": model_id, "outcome": "success", "estimated_tokens": estimated_tokens}
        if prompt_tokens is not None:
            payload["prompt_tokens"] = prompt_tokens
        if completion_tokens is not None:
            payload["completion_tokens"] = completion_tokens
        if cached_tokens is not None:
            payload["cached_tokens"] = cached_tokens
        if actual_tokens is not None:
            payload["actual_tokens"] = actual_tokens
        if remaining_requests is not None:
            payload["remaining_requests"] = remaining_requests
        if remaining_tokens is not None:
            payload["remaining_tokens"] = remaining_tokens
        if self._sync_provider_limits:
            if limit_requests is not None:
                payload["limit_requests"] = limit_requests
            if limit_tokens is not None:
                payload["limit_tokens"] = limit_tokens
            payload["sync_limits"] = True
        if brand_key_id is not None:
            payload["brand_key_id"] = brand_key_id
        if actual_cost_usd is not None:
            # Real, provider-reported cost (e.g. OpenRouter's `usage.cost`) — the server returns
            # this verbatim instead of estimating from the catalog price. Never pass a locally
            # computed estimate here, only a genuine provider-reported figure.
            payload["actual_cost_usd"] = actual_cost_usd
        if response_time_ms is not None:
            # Observed end-to-end wall-clock time of the actual provider call. Feeds a live
            # per-(model, key) EWMA server-side that scoring's latency term prefers over the
            # static catalog avg_latency_ms — lets slow-routing aggregators (e.g. OpenRouter,
            # Requesty) get penalised based on real observed behaviour.
            payload["response_time_ms"] = response_time_ms
        # Fire-and-forget: the LLM result is already in hand, no need to block
        # the caller while we send usage + remaining-limit data back to proviz.
        # Rate-limit/error reports remain synchronous (must arrive before retry select()).
        self._post_fire_and_forget("/report", payload)

    def report_rate_limit(
        self,
        model_id: str,
        error_type: str = "tpm",
        brand_key_id: Optional[str] = None,
    ) -> None:
        _logger.debug("report: model_id=%s outcome=rate_limit error_type=%s", model_id, error_type)
        payload: dict = {
            "model_id": model_id,
            "outcome": "rate_limit",
            "error_type": error_type,
        }
        if brand_key_id is not None:
            payload["brand_key_id"] = brand_key_id
        # No response_time_ms here: a 429 rejection is typically near-instant and doesn't
        # reflect actual generation speed, so it's not a meaningful latency sample.
        self._post("/report", payload)

    def report_error(
        self,
        model_id: str,
        error_type: str = "other",
        brand_key_id: Optional[str] = None,
        response_time_ms: Optional[int] = None,
    ) -> None:
        _logger.debug(
            "report: model_id=%s outcome=error error_type=%s response_time_ms=%s",
            model_id, error_type, response_time_ms,
        )
        payload: dict = {
            "model_id": model_id,
            "outcome": "error",
            "error_type": error_type,
        }
        if brand_key_id is not None:
            payload["brand_key_id"] = brand_key_id
        if response_time_ms is not None:
            # A genuine error (including a timeout, where elapsed is close to the timeout
            # ceiling) is a real data point about how slow this provider was to respond.
            payload["response_time_ms"] = response_time_ms
        self._post("/report", payload)

    def call(
        self,
        step: str,
        fn: Callable[[ModelCandidate], Any],
        *,
        estimated_tokens: int = 0,
        requires_fn_call: bool = False,
        requires_json_mode: bool = False,
        quality_min: float = 0.0,
        exclude_ids: Optional[list[str]] = None,
        categories: Optional[list[str]] = None,
        languages: Optional[list[str]] = None,
        error_classifier: Optional[Callable[[Exception], tuple[str, str]]] = None,
        max_wait_secs: float = 0.0,
        require_no_training: bool = False,
        cost_weight: Optional[float] = None,
        latency_weight: Optional[float] = None,
        quality_weight: Optional[float] = None,
    ) -> CallResult:
        """Select a model, call fn(candidate), report the outcome, and retry on failure.

        fn receives a ModelCandidate and must return the raw LLM response.
        Retries automatically until a model succeeds or AllModelsExhausted is raised.

        max_wait_secs: when all models are transiently exhausted (quota full), sleep for
        the server-supplied retry_after_ms hint and try again until this budget is spent.
        Set to 0 (default) to raise AllModelsExhausted immediately on exhaustion.

        require_no_training: only select models the source explicitly confirms don't train on
        submitted data. Note this only affects model *selection* — if `fn` calls OpenRouter
        directly, you must also pass OpenRouter's `provider={"data_collection": "deny", "zdr":
        True}` yourself inside `fn`, since proviz-elekto doesn't construct the request here (it
        does for `call_litellm`/`complete`, which handle this automatically).
        """
        classifier = error_classifier or _classify_error
        # Models that won't be blocked server-side (parse errors have TTL=0)
        permanent_skip: list[str] = list(exclude_ids or [])
        wait_deadline = time.monotonic() + max_wait_secs if max_wait_secs > 0 else None

        attempt = 0
        while True:
            attempt += 1
            # Pass the remaining wall-clock budget as max_wait_ms so the server can
            # sleep-and-retry internally before returning 409, saving a round-trip.
            server_wait_ms: Optional[int] = None
            if wait_deadline is not None:
                remaining_budget = wait_deadline - time.monotonic()
                if remaining_budget > 0:
                    server_wait_ms = int(remaining_budget * 1000)
            try:
                candidate = self.select(
                    step=step,
                    estimated_tokens=estimated_tokens,
                    requires_fn_call=requires_fn_call,
                    requires_json_mode=requires_json_mode,
                    quality_min=quality_min,
                    exclude_ids=permanent_skip,
                    categories=categories,
                    languages=languages,
                    max_wait_ms=server_wait_ms,
                    require_no_training=require_no_training,
                    cost_weight=cost_weight,
                    latency_weight=latency_weight,
                    quality_weight=quality_weight,
                )
            except AllModelsExhausted as e:
                if wait_deadline is not None and e.retry_after_ms > 0:
                    remaining = wait_deadline - time.monotonic()
                    base_wait = min(e.retry_after_ms / 1000.0, remaining)
                    if base_wait > 0:
                        # Jitter ±20% so concurrent workers don't all retry simultaneously.
                        wait = base_wait * random.uniform(0.8, 1.2)
                        wait = min(wait, remaining)
                        _logger.debug(
                            "step=%s all models exhausted, retrying in %.2fs "
                            "(retry_after_ms=%d, jittered, attempt=%d)",
                            step, wait, e.retry_after_ms, attempt,
                        )
                        time.sleep(wait)
                        continue
                raise
            _logger.debug(
                "call attempt=%d step=%s model=%s/%s",
                attempt, step, candidate.brand_slug, candidate.model_slug,
            )
            call_started = time.monotonic()
            try:
                response = fn(candidate)
                response_time_ms = int((time.monotonic() - call_started) * 1000)
                prompt, completion, total = _extract_usage(response)
                rem_req, rem_tok, lim_req, lim_tok = _extract_provider_limits(response)
                if rem_req is None and rem_tok is None:
                    _logger.debug(
                        "call: no rate-limit headers for %s/%s (known keys: %s)",
                        candidate.brand_slug, candidate.model_slug,
                        list(_collect_response_headers(response).keys()) or "none",
                    )
                provider_cost_usd = _extract_provider_cost(response)
                cached = _extract_cached_tokens(response)
                actual_cost_usd = (
                    provider_cost_usd
                    if provider_cost_usd is not None
                    else _compute_cost(
                        candidate, prompt, completion, self._fx_rates(), cached_tokens=cached
                    )
                )
                self.report_success(
                    candidate.model_id,
                    estimated_tokens=estimated_tokens,
                    prompt_tokens=prompt if prompt else None,
                    completion_tokens=completion if completion else None,
                    cached_tokens=cached or None,
                    remaining_requests=rem_req,
                    remaining_tokens=rem_tok,
                    limit_requests=lim_req,
                    limit_tokens=lim_tok,
                    brand_key_id=candidate.brand_key_id,
                    actual_cost_usd=provider_cost_usd,
                    response_time_ms=response_time_ms,
                )
                _logger.debug(
                    "call success: model=%s/%s prompt=%d completion=%d total=%d "
                    "cost_usd=%s remaining_req=%s remaining_tok=%s limit_req=%s limit_tok=%s",
                    candidate.brand_slug, candidate.model_slug, prompt, completion, total,
                    actual_cost_usd, rem_req, rem_tok, lim_req, lim_tok,
                )
                return CallResult(
                    response=response,
                    candidate=candidate,
                    provider=candidate.brand_slug,
                    prompt_tokens=prompt,
                    completion_tokens=completion,
                    total_tokens=total,
                    actual_cost_usd=actual_cost_usd,
                    cached_tokens=cached,
                )
            except AllModelsExhausted:
                raise
            except Exception as exc:
                outcome, error_type = classifier(exc)
                _logger.debug(
                    "call error: model=%s/%s outcome=%s error_type=%s exc=%s",
                    candidate.brand_slug, candidate.model_slug, outcome, error_type, exc,
                )
                if outcome == "rate_limit":
                    self.report_rate_limit(candidate.model_id, error_type, brand_key_id=candidate.brand_key_id)
                else:
                    elapsed_ms = int((time.monotonic() - call_started) * 1000)
                    self.report_error(
                        candidate.model_id, error_type, brand_key_id=candidate.brand_key_id,
                        response_time_ms=elapsed_ms,
                    )
                if error_type == "parse":
                    permanent_skip.append(candidate.model_id)

    def call_litellm(
        self,
        step: str,
        messages: list[dict],
        *,
        estimated_tokens: Optional[int] = None,
        requires_fn_call: bool = False,
        requires_json_mode: bool = False,
        quality_min: float = 0.0,
        exclude_ids: Optional[list[str]] = None,
        categories: Optional[list[str]] = None,
        languages: Optional[list[str]] = None,
        error_classifier: Optional[Callable[[Exception], tuple[str, str]]] = None,
        max_wait_secs: float = 0.0,
        require_no_training: bool = False,
        cost_weight: Optional[float] = None,
        latency_weight: Optional[float] = None,
        quality_weight: Optional[float] = None,
        **litellm_kwargs: Any,
    ) -> CallResult:
        """call() with built-in LiteLLM integration.

        Requires: pip install proviz-elekto[litellm]
        Model string and API key are derived automatically from the selected candidate.
        Selection parameters (categories, requires_fn_call, etc.) are used for model
        selection and are NOT forwarded to litellm.completion(). Any remaining kwargs
        are forwarded to litellm.completion().

        max_wait_secs: budget for retrying when all models are transiently exhausted.

        require_no_training: only select models the source explicitly confirms don't train on
        submitted data (see ModelCandidate.trains_on_data). Since OpenRouter doesn't report this
        per-model at all, when set this also sends OpenRouter's own request-level opt-out
        (`provider={"data_collection": "deny", "zdr": True}`) on every call — a harmless no-op
        for other providers, which ignore the unrecognized field.
        """
        try:
            import litellm
        except ImportError:
            raise ProvizError(
                "litellm is not installed. Run: pip install proviz-elekto[litellm]"
            ) from None

        def fn(candidate: ModelCandidate) -> Any:
            kwargs = dict(litellm_kwargs)
            if require_no_training:
                kwargs["provider"] = {"data_collection": "deny", "zdr": True}
            model_id, target_kwargs = _litellm_target(candidate)
            return litellm.completion(
                model=model_id,
                messages=messages,
                api_key=os.environ.get(candidate.api_key_env or "", "") or None,
                **target_kwargs,
                **kwargs,
            )

        tokens = estimated_tokens if estimated_tokens is not None else _estimate_tokens(messages)
        return self.call(
            step, fn,
            estimated_tokens=tokens,
            requires_fn_call=requires_fn_call,
            requires_json_mode=requires_json_mode,
            quality_min=quality_min,
            exclude_ids=exclude_ids,
            categories=categories,
            languages=languages,
            error_classifier=error_classifier,
            max_wait_secs=max_wait_secs,
            require_no_training=require_no_training,
            cost_weight=cost_weight,
            latency_weight=latency_weight,
            quality_weight=quality_weight,
        )

    def call_litellm_tool_loop(
        self,
        step: str,
        messages: list,
        tools: list,
        tool_executor: Callable[[str, str], str],
        *,
        estimated_tokens: int = 0,
        max_iterations: int = 5,
        tool_choice: str = "auto",
        requires_json_mode: bool = False,
        quality_min: float = 0.0,
        exclude_ids: Optional[list[str]] = None,
        categories: Optional[list[str]] = None,
        languages: Optional[list[str]] = None,
        group_id: Optional[str] = None,
        group_name: Optional[str] = None,
        error_classifier: Optional[Callable[[Exception], tuple[str, str]]] = None,
        max_wait_secs: float = 0.0,
        require_no_training: bool = False,
        cost_weight: Optional[float] = None,
        latency_weight: Optional[float] = None,
        quality_weight: Optional[float] = None,
        **litellm_kwargs: Any,
    ) -> Optional["CallResult"]:
        """select → [litellm.completion → execute tools → append → repeat] → report_success

        Owns the full multi-turn tool-use loop and handles token accumulation + reporting
        internally. Tokens are accumulated across all iterations; a single report_success
        fires after the final round with aggregate prompt/completion/remaining data so the
        anchor floor and sliding-window correction activate correctly.

        tool_executor: callable(tool_name, arguments_json) → result_str
            Called once per tool call in each iteration. Exceptions propagate and cause
            the whole attempt to be reported as an error.

        Returns CallResult where .response is the final message content (str), or None if
        max_iterations is reached without a non-tool-call response.

        require_no_training: same semantics as call_litellm's — filters selection to models
        confirmed not to train, and sends OpenRouter's provider opt-out on every iteration's call.

        Requires: pip install proviz-elekto[litellm]
        """
        try:
            import litellm
        except ImportError:
            raise ProvizError(
                "litellm is not installed. Run: pip install proviz-elekto[litellm]"
            ) from None

        classifier = error_classifier or _classify_error
        permanent_skip: list[str] = list(exclude_ids or [])
        wait_deadline = time.monotonic() + max_wait_secs if max_wait_secs > 0 else None

        _SELECTION_KEYS = frozenset({
            "step", "estimated_tokens", "requires_fn_call", "requires_json_mode",
            "quality_min", "exclude_ids", "categories", "languages", "group_id", "group_name",
            "use_member_priority", "max_wait_secs", "max_wait_ms",
            "cost_weight", "latency_weight", "quality_weight",
        })
        litellm_kwargs = {k: v for k, v in litellm_kwargs.items() if k not in _SELECTION_KEYS}

        attempt = 0
        while True:
            attempt += 1
            server_wait_ms: Optional[int] = None
            if wait_deadline is not None:
                remaining_budget = wait_deadline - time.monotonic()
                if remaining_budget > 0:
                    server_wait_ms = int(remaining_budget * 1000)
            try:
                candidate = self.select(
                    step=step,
                    estimated_tokens=estimated_tokens,
                    requires_fn_call=True,
                    requires_json_mode=requires_json_mode,
                    quality_min=quality_min,
                    exclude_ids=permanent_skip,
                    categories=categories,
                    languages=languages,
                    group_id=group_id,
                    group_name=group_name,
                    max_wait_ms=server_wait_ms,
                    require_no_training=require_no_training,
                    cost_weight=cost_weight,
                    latency_weight=latency_weight,
                    quality_weight=quality_weight,
                )
            except AllModelsExhausted as e:
                if wait_deadline is not None and e.retry_after_ms > 0:
                    remaining = wait_deadline - time.monotonic()
                    base_wait = min(e.retry_after_ms / 1000.0, remaining)
                    if base_wait > 0:
                        wait = base_wait * random.uniform(0.8, 1.2)
                        wait = min(wait, remaining)
                        _logger.debug(
                            "step=%s all models exhausted, retrying in %.2fs "
                            "(retry_after_ms=%d, jittered, attempt=%d)",
                            step, wait, e.retry_after_ms, attempt,
                        )
                        time.sleep(wait)
                        continue
                raise

            _logger.debug(
                "call_litellm_tool_loop attempt=%d step=%s model=%s/%s",
                attempt, step, candidate.brand_slug, candidate.model_slug,
            )

            total_prompt = total_completion = total_tokens = total_cached = 0
            total_provider_cost: Optional[float] = None
            last_rem_req: Optional[int] = None
            last_rem_tok: Optional[int] = None
            last_lim_req: Optional[int] = None
            last_lim_tok: Optional[int] = None
            current_messages = list(messages)

            iter_kwargs = dict(litellm_kwargs)
            if require_no_training:
                iter_kwargs["provider"] = {"data_collection": "deny", "zdr": True}

            loop_started = time.monotonic()
            _tl_model_id, _tl_target_kwargs = _litellm_target(candidate)
            try:
                for iteration in range(max_iterations):
                    response = litellm.completion(
                        model=_tl_model_id,
                        messages=current_messages,
                        tools=tools,
                        tool_choice=tool_choice,
                        api_key=os.environ.get(candidate.api_key_env or "", "") or None,
                        **_tl_target_kwargs,
                        **iter_kwargs,
                    )
                    pt, ct, tot = _extract_usage(response)
                    last_rem_req, last_rem_tok, last_lim_req, last_lim_tok = _extract_provider_limits(response)
                    if last_rem_req is None and last_rem_tok is None:
                        _logger.debug(
                            "call_litellm_tool_loop: no rate-limit headers found for %s/%s "
                            "(known header keys: %s)",
                            candidate.brand_slug, candidate.model_slug,
                            list(_collect_response_headers(response).keys()) or "none",
                        )
                    total_prompt += pt
                    total_completion += ct
                    total_tokens += tot
                    total_cached += _extract_cached_tokens(response)
                    iter_cost = _extract_provider_cost(response)
                    if iter_cost is not None:
                        total_provider_cost = (total_provider_cost or 0.0) + iter_cost

                    msg = response.choices[0].message
                    tool_calls = getattr(msg, "tool_calls", None) or []

                    if not tool_calls:
                        actual_cost_usd = (
                            total_provider_cost
                            if total_provider_cost is not None
                            else _compute_cost(
                                candidate, total_prompt, total_completion, self._fx_rates(),
                                cached_tokens=total_cached,
                            )
                        )
                        self.report_success(
                            candidate.model_id,
                            estimated_tokens=estimated_tokens,
                            actual_tokens=total_tokens,
                            prompt_tokens=total_prompt if total_prompt else None,
                            completion_tokens=total_completion if total_completion else None,
                            cached_tokens=total_cached or None,
                            remaining_requests=last_rem_req,
                            remaining_tokens=last_rem_tok,
                            limit_requests=last_lim_req,
                            limit_tokens=last_lim_tok,
                            brand_key_id=candidate.brand_key_id,
                            actual_cost_usd=total_provider_cost,
                            # Aggregate wall-clock across every iteration of this tool loop —
                            # matches how tokens/cost are already accumulated across iterations.
                            response_time_ms=int((time.monotonic() - loop_started) * 1000),
                        )
                        _logger.debug(
                            "call_litellm_tool_loop success: model=%s/%s iterations=%d "
                            "prompt=%d completion=%d cost_usd=%s",
                            candidate.brand_slug, candidate.model_slug, iteration + 1,
                            total_prompt, total_completion, actual_cost_usd,
                        )
                        return CallResult(
                            response=msg.content or "",
                            candidate=candidate,
                            provider=candidate.brand_slug,
                            prompt_tokens=total_prompt,
                            completion_tokens=total_completion,
                            total_tokens=total_tokens,
                            actual_cost_usd=actual_cost_usd,
                            cached_tokens=total_cached,
                        )

                    current_messages = current_messages + [msg]
                    for tc in tool_calls:
                        result = tool_executor(tc.function.name, tc.function.arguments)
                        current_messages.append({
                            "role": "tool",
                            "tool_call_id": tc.id,
                            "content": result,
                        })

                _logger.warning(
                    "call_litellm_tool_loop: max_iterations=%d reached without final answer "
                    "for step=%s model=%s/%s",
                    max_iterations, step, candidate.brand_slug, candidate.model_slug,
                )
                self.report_error(
                    candidate.model_id, "other", brand_key_id=candidate.brand_key_id,
                    response_time_ms=int((time.monotonic() - loop_started) * 1000),
                )
                return None

            except AllModelsExhausted:
                raise
            except Exception as exc:
                outcome, error_type = classifier(exc)
                _logger.debug(
                    "call_litellm_tool_loop error: model=%s/%s outcome=%s error_type=%s exc=%s",
                    candidate.brand_slug, candidate.model_slug, outcome, error_type, exc,
                )
                if outcome == "rate_limit":
                    self.report_rate_limit(candidate.model_id, error_type, brand_key_id=candidate.brand_key_id)
                else:
                    self.report_error(
                        candidate.model_id, error_type, brand_key_id=candidate.brand_key_id,
                        response_time_ms=int((time.monotonic() - loop_started) * 1000),
                    )
                if error_type == "parse":
                    permanent_skip.append(candidate.model_id)

    def complete_batch(
        self,
        step: str,
        messages: list[dict],
        *,
        group_name: Optional[str] = None,
        categories: Optional[list[str]] = None,
        languages: Optional[list[str]] = None,
        temperature: Optional[float] = None,
        max_tokens: Optional[int] = None,
        timeout_secs: float = 300.0,
    ) -> CompleteResult:
        """Batch variant of complete(): select a Mistral model, queue via Mistral Batch API.

        Submits to the server-side batch queue (``/batch/submit``), then polls
        ``/batch/result/{id}`` until the Mistral batch job completes.  Blocks for up
        to *timeout_secs* (default 10 min) before raising ``BatchTimeoutError``.

        Only Mistral text/code models are eligible — the server enforces this.
        Cost is ~50% of the synchronous path due to Mistral's batch pricing.

        Returns a :class:`CompleteResult` with the same shape as :meth:`complete`.
        """
        from . import batch as batch_module

        select_kwargs: dict = {}
        if group_name is not None:
            select_kwargs["group_name"] = group_name
        if categories:
            select_kwargs["categories"] = categories
        if languages:
            select_kwargs["languages"] = languages

        queue = batch_module.BatchQueue(proviz=self, step=step, **select_kwargs)

        extra: dict = {}
        if temperature is not None:
            extra["temperature"] = temperature
        if max_tokens is not None:
            extra["max_tokens"] = max_tokens

        job = queue.submit(messages, **extra)
        result = job.result(timeout=timeout_secs)

        raw_model: str = result.body.get("model", "")
        content: str = result.content or ""

        return CompleteResult(
            text=content,
            model=raw_model,
            brand="mistral",
            prompt_tokens=result.prompt_tokens,
            completion_tokens=result.completion_tokens,
            cost_usd=result.actual_cost_usd,
        )

    def create_batch_queue(self, step: str, **select_kwargs: Any) -> "batch_module.BatchQueue":
        """Create a BatchQueue that routes requests through Mistral's Batch API (50% cost reduction).

        Requests submitted to the queue are forwarded to the server-side batch queue.
        The server accumulates them from all workers and flushes as a single Mistral
        batch job after the configured window (``--batch-window-secs``, default 60s).

        Only Mistral text/code models are eligible. The server enforces this.

        Args:
            step: Step name for model selection.
            **select_kwargs: Forwarded to ``/batch/submit`` (e.g. ``quality_min``,
                ``group_name``, ``requires_fn_call``).

        Example::

            queue = pz.create_batch_queue("classify")
            jobs = [queue.submit([{"role": "user", "content": t}]) for t in texts]
            for job in jobs:
                print(job.result(timeout=300).content)
        """
        from . import batch as batch_module
        return batch_module.BatchQueue(proviz=self, step=step, **select_kwargs)

    def health(self) -> dict:
        req = urllib.request.Request(f"{self._base}/health")
        with urllib.request.urlopen(req, timeout=self._timeout) as resp:
            return json.loads(resp.read())

    def reload(self) -> dict:
        return self._post("/catalog/reload", {})
