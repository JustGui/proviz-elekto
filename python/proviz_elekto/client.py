from __future__ import annotations

import atexit
import json
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


class ProvizError(Exception):
    pass


class AllModelsExhausted(ProvizError):
    def __init__(self, step: str, tried: int):
        super().__init__(f"all models exhausted for step '{step}' (tried {tried})")
        self.step = step
        self.tried = tried


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
    ) -> ModelCandidate:
        r = self._post("/select", {
            "step": step,
            "estimated_tokens": estimated_tokens,
            "requires_fn_call": requires_fn_call,
            "requires_json_mode": requires_json_mode,
            "quality_min": quality_min,
            "exclude_ids": exclude_ids or [],
            "categories": categories or [],
        })
        return ModelCandidate(
            model_id=r["model_id"],
            brand_slug=r["brand_slug"],
            model_slug=r["model_slug"],
            api_key_env=r.get("api_key_env"),
            max_context_tokens=r["max_context_tokens"],
            supports_function_calling=r["supports_function_calling"],
            supports_json_mode=r["supports_json_mode"],
            estimated_input_cost_usd=r.get("estimated_input_cost_usd"),
        )

    def report_success(self, model_id: str) -> None:
        self._post("/report", {"model_id": model_id, "outcome": "success"})

    def report_rate_limit(self, model_id: str, error_type: str = "tpm") -> None:
        self._post("/report", {
            "model_id": model_id,
            "outcome": "rate_limit",
            "error_type": error_type,
        })

    def report_error(self, model_id: str, error_type: str = "other") -> None:
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
    ) -> CallResult:
        """Select a model, call fn(candidate), report the outcome, and retry on failure.

        fn receives a ModelCandidate and must return the raw LLM response.
        Retries automatically until a model succeeds or AllModelsExhausted is raised.
        """
        classifier = error_classifier or _classify_error
        # Models that won't be blocked server-side (parse errors have TTL=0)
        permanent_skip: list[str] = list(exclude_ids or [])

        while True:
            candidate = self.select(
                step=step,
                estimated_tokens=estimated_tokens,
                requires_fn_call=requires_fn_call,
                requires_json_mode=requires_json_mode,
                quality_min=quality_min,
                exclude_ids=permanent_skip,
                categories=categories,
            )
            try:
                response = fn(candidate)
                self.report_success(candidate.model_id)
                prompt, completion, total = _extract_usage(response)
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
        **litellm_kwargs: Any,
    ) -> CallResult:
        """call() with built-in LiteLLM integration.

        Requires: pip install proviz-elekto[litellm]
        Model string and API key are derived automatically from the selected candidate.
        Selection parameters (categories, requires_fn_call, etc.) are used for model
        selection and are NOT forwarded to litellm.completion(). Any remaining kwargs
        are forwarded to litellm.completion().
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
        )

    def health(self) -> dict:
        req = urllib.request.Request(f"{self._base}/health")
        with urllib.request.urlopen(req, timeout=self._timeout) as resp:
            return json.loads(resp.read())

    def reload(self) -> dict:
        return self._post("/catalog/reload", {})
