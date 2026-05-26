from __future__ import annotations

import atexit
import json
import logging
import os
import select as _select
import shutil
import socket
import subprocess
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


@dataclass
class CallResult:
    response: Any
    candidate: ModelCandidate
    provider: str
    prompt_tokens: int
    completion_tokens: int
    total_tokens: int


def _classify_error(exc: Exception) -> tuple[str, str]:
    status = getattr(exc, "status_code", None)
    cls = type(exc).__name__

    if status == 429 or "RateLimit" in cls:
        msg = str(exc).lower()
        if "day" in msg or "tpd" in msg:
            return "rate_limit", "tpd"
        if "token" in msg or "tpm" in msg:
            return "rate_limit", "tpm"
        return "rate_limit", "rpm"

    if status in (401, 403) or "Auth" in cls:
        return "error", "auth"

    if "Timeout" in cls or status == 408:
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


def _estimate_tokens(messages: list[dict]) -> int:
    total = sum(len(str(m.get("content", ""))) for m in messages)
    return max(1, total // 4)


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
    ):
        self._proc: Optional[subprocess.Popen] = None
        host = os.environ.get("PROVIZ_HOST", host)
        port = int(os.environ.get("PROVIZ_PORT", port))
        self._host = host
        self._port = port
        self._base = f"http://{host}:{port}"
        self._timeout = timeout

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

    def _post(self, path: str, body: dict) -> dict:
        data = json.dumps(body).encode()
        req = urllib.request.Request(
            f"{self._base}{path}",
            data=data,
            headers={"Content-Type": "application/json"},
            method="POST",
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
            raise ProvizError(f"HTTP {e.code}: {payload}") from e

    # ── Public API ────────────────────────────────────────────────────────────

    def select(
        self,
        step: str,
        estimated_tokens: int,
        requires_fn_call: bool = False,
        requires_json_mode: bool = False,
        quality_min: float = 0.0,
        exclude_ids: Optional[list[str]] = None,
        categories: Optional[list[str]] = None,
        group_id: Optional[str] = None,
        group_name: Optional[str] = None,
        use_member_priority: bool = True,
    ) -> ModelCandidate:
        payload: dict = {
            "step": step,
            "estimated_tokens": estimated_tokens,
            "requires_fn_call": requires_fn_call,
            "requires_json_mode": requires_json_mode,
            "quality_min": quality_min,
            "exclude_ids": exclude_ids or [],
            "categories": categories or [],
            "use_member_priority": use_member_priority,
        }
        if group_id is not None:
            payload["group_id"] = group_id
        if group_name is not None:
            payload["group_name"] = group_name
        _logger.debug(
            "select request: step=%s estimated_tokens=%d group_name=%s group_id=%s",
            step, estimated_tokens, group_name, group_id,
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
        )
        _logger.debug(
            "select response: model=%s brand=%s cost_usd=%s",
            candidate.model_slug, candidate.brand_slug, candidate.estimated_input_cost_usd,
        )
        return candidate

    def report_success(self, model_id: str, estimated_tokens: int = 0, actual_tokens: Optional[int] = None) -> None:
        _logger.debug("report: model_id=%s outcome=success actual_tokens=%s", model_id, actual_tokens)
        payload: dict = {"model_id": model_id, "outcome": "success", "estimated_tokens": estimated_tokens}
        if actual_tokens is not None:
            payload["actual_tokens"] = actual_tokens
        self._post("/report", payload)

    def report_rate_limit(self, model_id: str, error_type: str = "tpm") -> None:
        _logger.debug("report: model_id=%s outcome=rate_limit error_type=%s", model_id, error_type)
        self._post("/report", {
            "model_id": model_id,
            "outcome": "rate_limit",
            "error_type": error_type,
        })

    def report_error(self, model_id: str, error_type: str = "other") -> None:
        _logger.debug("report: model_id=%s outcome=error error_type=%s", model_id, error_type)
        self._post("/report", {
            "model_id": model_id,
            "outcome": "error",
            "error_type": error_type,
        })

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
        error_classifier: Optional[Callable[[Exception], tuple[str, str]]] = None,
        max_wait_secs: float = 0.0,
    ) -> CallResult:
        """Select a model, call fn(candidate), report the outcome, and retry on failure.

        fn receives a ModelCandidate and must return the raw LLM response.
        Retries automatically until a model succeeds or AllModelsExhausted is raised.

        max_wait_secs: when all models are transiently exhausted (quota full), sleep for
        the server-supplied retry_after_ms hint and try again until this budget is spent.
        Set to 0 (default) to raise AllModelsExhausted immediately on exhaustion.
        """
        classifier = error_classifier or _classify_error
        # Models that won't be blocked server-side (parse errors have TTL=0)
        permanent_skip: list[str] = list(exclude_ids or [])
        wait_deadline = time.monotonic() + max_wait_secs if max_wait_secs > 0 else None

        attempt = 0
        while True:
            attempt += 1
            try:
                candidate = self.select(
                    step=step,
                    estimated_tokens=estimated_tokens,
                    requires_fn_call=requires_fn_call,
                    requires_json_mode=requires_json_mode,
                    quality_min=quality_min,
                    exclude_ids=permanent_skip,
                    categories=categories,
                )
            except AllModelsExhausted as e:
                if wait_deadline is not None and e.retry_after_ms > 0:
                    remaining = wait_deadline - time.monotonic()
                    wait = min(e.retry_after_ms / 1000.0, remaining)
                    if wait > 0:
                        _logger.debug(
                            "step=%s all models exhausted, retrying in %.1fs "
                            "(retry_after_ms=%d, attempt=%d)",
                            step, wait, e.retry_after_ms, attempt,
                        )
                        time.sleep(wait)
                        continue
                raise
            _logger.debug(
                "call attempt=%d step=%s model=%s/%s",
                attempt, step, candidate.brand_slug, candidate.model_slug,
            )
            try:
                response = fn(candidate)
                prompt, completion, total = _extract_usage(response)
                self.report_success(candidate.model_id, estimated_tokens=estimated_tokens, actual_tokens=total)
                _logger.debug(
                    "call success: model=%s/%s prompt=%d completion=%d total=%d",
                    candidate.brand_slug, candidate.model_slug, prompt, completion, total,
                )
                return CallResult(
                    response=response,
                    candidate=candidate,
                    provider=candidate.brand_slug,
                    prompt_tokens=prompt,
                    completion_tokens=completion,
                    total_tokens=total,
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
                    self.report_rate_limit(candidate.model_id, error_type)
                else:
                    self.report_error(candidate.model_id, error_type)
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
        error_classifier: Optional[Callable[[Exception], tuple[str, str]]] = None,
        max_wait_secs: float = 0.0,
        **litellm_kwargs: Any,
    ) -> CallResult:
        """call() with built-in LiteLLM integration.

        Requires: pip install proviz-elekto[litellm]
        Model string and API key are derived automatically from the selected candidate.
        Selection parameters (categories, requires_fn_call, etc.) are used for model
        selection and are NOT forwarded to litellm.completion(). Any remaining kwargs
        are forwarded to litellm.completion().

        max_wait_secs: budget for retrying when all models are transiently exhausted.
        """
        try:
            import litellm
        except ImportError:
            raise ProvizError(
                "litellm is not installed. Run: pip install proviz-elekto[litellm]"
            ) from None

        def fn(candidate: ModelCandidate) -> Any:
            return litellm.completion(
                model=f"{candidate.brand_slug}/{candidate.model_slug}",
                messages=messages,
                api_key=os.environ.get(candidate.api_key_env or "", "") or None,
                **litellm_kwargs,
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
            error_classifier=error_classifier,
            max_wait_secs=max_wait_secs,
        )

    def health(self) -> dict:
        req = urllib.request.Request(f"{self._base}/health")
        with urllib.request.urlopen(req, timeout=self._timeout) as resp:
            return json.loads(resp.read())

    def reload(self) -> dict:
        return self._post("/catalog/reload", {})
