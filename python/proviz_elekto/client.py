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
from typing import Optional


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


def _find_binary() -> str:
    """Locate the proviz-server binary bundled with the package or on PATH."""
    pkg_dir = os.path.dirname(__file__)
    for name in ("proviz-server", "proviz-server.exe"):
        bundled = os.path.join(pkg_dir, name)
        if os.path.isfile(bundled) and os.access(bundled, os.X_OK):
            return bundled
    on_path = shutil.which("proviz-server")
    if on_path:
        return on_path
    raise ProvizError(
        "proviz-server binary not found. "
        "Reinstall the package or build from source: cargo build --release --bin proviz-server"
    )


class ProvizElekto:
    """
    Smart LLM model router.

    Automatically starts the proviz-server binary as a subprocess on first use.
    The subprocess is shut down cleanly when the Python process exits.

    Usage:
        pz = ProvizElekto(database_url="postgresql://...")
        candidate = pz.select(step="verdict", estimated_tokens=2500)
        ...
        pz.report_success(candidate.model_id)
    """

    def __init__(
        self,
        database_url: Optional[str] = None,
        db_path: str = "proviz.db",
        port: int = 0,
        timeout: float = 5.0,
        startup_timeout: float = 10.0,
    ):
        self._proc: Optional[subprocess.Popen] = None
        self._port = port
        self._base = f"http://localhost:{port}"
        self._timeout = timeout

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
            with socket.create_connection(("localhost", self._port), timeout=0.5):
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
        self._base = f"http://localhost:{actual_port}"

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
    ) -> ModelCandidate:
        r = self._post("/select", {
            "step": step,
            "estimated_tokens": estimated_tokens,
            "requires_fn_call": requires_fn_call,
            "requires_json_mode": requires_json_mode,
            "quality_min": quality_min,
            "exclude_ids": exclude_ids or [],
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

    def health(self) -> dict:
        req = urllib.request.Request(f"{self._base}/health")
        with urllib.request.urlopen(req, timeout=self._timeout) as resp:
            return json.loads(resp.read())

    def reload(self) -> dict:
        return self._post("/catalog/reload", {})
