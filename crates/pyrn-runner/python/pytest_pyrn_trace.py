"""Standalone Python 3.12+ PEP 669 `sys.monitoring` + PEP 578 `sys.addaudithook` +
transparent `os.environ` read-proxy TIA tracer plugin for pytest (`pytest_pyrn_trace`).

Matches `pyrn_core::dynamic::DynamicTraceFile` / `DynamicTestTrace` schema identically.
Zero monkeypatching of `builtins.open`, `io.open`, `subprocess.Popen`, or `socket`.
Read-only safe on target repositories: pass `--pyrn-trace-out <external_path.json>`.
"""

from __future__ import annotations

import contextlib
import json
import os
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import pytest

_TAINTED_MARKERS = frozenset(
    {"real_ray", "live_cluster", "postgresql", "compiled_graph_opt_in"}
)
_TAINTED_FIXTURES = frozenset({"ray_cluster", "live_ray_cluster", "live_server"})
_AUDIT_EVENTS = frozenset(
    {"open", "os.listdir", "os.scandir", "subprocess.Popen", "socket.connect"}
)
_IGNORED_TRACE_ENV_KEYS = frozenset(
    {
        "TMP",
        "TEMP",
        "TMPDIR",
        "PYTEST_CURRENT_TEST",
        "PYRN_WORKER_ID",
        "PYRN_PORT_START",
        "PYRN_PORT_END",
        "DJANGO_LIVE_TEST_SERVER_ADDRESS",
        "COVERAGE_FILE",
        "COVERAGE_CORE",
        "COV_CORE_SOURCE",
        "COV_CORE_CONFIG",
        "COV_CORE_DATAFILE",
    }
)


def _normalize_repo_rel(path_like: object, root_str: str, root_lower: str) -> str | None:
    if not isinstance(path_like, (str, os.PathLike)):
        return None
    try:
        raw = os.fspath(path_like)
    except TypeError:
        return None
    if isinstance(raw, bytes):
        try:
            raw = raw.decode("utf-8", errors="ignore")
        except Exception:
            return None
    if not raw or raw.startswith("<"):
        return None
    abs_path = os.path.abspath(raw)
    if sys.platform == "win32":
        if not abs_path.lower().startswith(root_lower):
            return None
    else:
        if not abs_path.startswith(root_str):
            return None
    rel = abs_path[len(root_str) :].lstrip("\\/").replace("\\", "/")
    if not rel or rel.startswith(".venv/") or "/.venv/" in rel or rel.startswith(".git/"):
        return None
    return rel


@dataclass
class _ActiveTestCapture:
    nodeid: str
    file_path: str
    executed_py_files: set[str] = field(default_factory=set)
    executed_symbols: set[str] = field(default_factory=set)
    read_assets: set[str] = field(default_factory=set)
    listed_dirs: set[str] = field(default_factory=set)
    env_vars_read: dict[str, str | None] = field(default_factory=dict)
    taint_reasons: set[str] = field(default_factory=set)
    fixtures: list[str] = field(default_factory=list)
    markers: list[str] = field(default_factory=list)
    start_ns: int = 0
    duration_ms: float = 0.0
    outcome: str = "unknown"


_ACTIVE_TRACER: _PyrnTracer | None = None
_AUDIT_HOOK_INSTALLED = False
_ENVIRON_HOOK_INSTALLED = False


def _audit_hook(event: str, args: tuple[Any, ...]) -> None:
    tracer = _ACTIVE_TRACER
    if tracer is None or event not in _AUDIT_EVENTS:
        return
    cap = tracer.active
    if cap is None:
        return

    if event == "open":
        path_arg = args[0] if args else None
        mode = args[1] if len(args) > 1 and isinstance(args[1], str) else "r"
        if "w" not in mode and "x" not in mode and "a" not in mode:
            rel = _normalize_repo_rel(path_arg, tracer.root_str, tracer.root_lower)
            if rel is not None and not rel.endswith(".pyc"):
                cap.read_assets.add(rel)
    elif event in ("os.listdir", "os.scandir"):
        path_arg = args[0] if args else "."
        rel = _normalize_repo_rel(path_arg, tracer.root_str, tracer.root_lower)
        if rel is not None:
            cap.listed_dirs.add(rel)
    elif event == "subprocess.Popen":
        cap.taint_reasons.add("dynamic:subprocess.Popen")
        argv = args[1] if len(args) > 1 else ()
        if isinstance(argv, (list, tuple)):
            for part in argv:
                rel = _normalize_repo_rel(part, tracer.root_str, tracer.root_lower)
                if rel is not None:
                    cap.read_assets.add(rel)
    elif event == "socket.connect":
        cap.taint_reasons.add("dynamic:socket.connect")


def _install_environ_read_proxy() -> None:
    global _ENVIRON_HOOK_INSTALLED
    if _ENVIRON_HOOK_INSTALLED:
        return
    environ_cls = type(os.environ)
    orig = environ_cls.__getitem__

    def _traced_getitem(self: Any, key: Any) -> Any:
        tracer = _ACTIVE_TRACER
        if tracer is None or self is not os.environ:
            return orig(self, key)
        cap = tracer.active
        if cap is None:
            return orig(self, key)

        norm_key = (key.upper() if sys.platform == "win32" else key) if isinstance(key, str) else None
        try:
            val = orig(self, key)
        except KeyError:
            if norm_key is not None and norm_key not in _IGNORED_TRACE_ENV_KEYS:
                cap.env_vars_read.setdefault(norm_key, None)
            raise
        if norm_key is not None and norm_key not in _IGNORED_TRACE_ENV_KEYS:
            cap.env_vars_read[norm_key] = val if isinstance(val, str) else str(val)
        return val

    environ_cls.__getitem__ = _traced_getitem  # type: ignore[method-assign]
    _ENVIRON_HOOK_INSTALLED = True


class _PyrnTracer:
    def __init__(
        self,
        root: Path,
        out_path: Path,
        granularity: str = "file",
        capture_symbols: bool = False,
        enable_audit: bool = True,
    ) -> None:
        self.root = root.resolve()
        root_s = str(self.root)
        if not root_s.endswith(os.sep):
            root_s = root_s + os.sep
        self.root_str = root_s
        self.root_lower = root_s.lower()
        self.out_path = out_path
        self.granularity = granularity
        self.capture_symbols = capture_symbols
        self.enable_audit = enable_audit
        self.active: _ActiveTestCapture | None = None
        self.last_file_path: str | None = None
        self.file_py_accum: set[str] = set()
        self.file_sym_accum: set[str] = set()
        self.records: list[dict[str, Any]] = []
        self.session_start_ns = time.perf_counter_ns()
        self.tool_id: int | None = None
        self.backend = "none"
        self.restart_count = 0
        self._filename_cache: dict[str, str | None] = {}
        self._installed = False

    def _rel_cached(self, filename: str) -> str | None:
        cached = self._filename_cache.get(filename, False)
        if cached is not False:
            return cached  # type: ignore[return-value]
        rel = _normalize_repo_rel(filename, self.root_str, self.root_lower)
        self._filename_cache[filename] = rel
        return rel

    def install(self) -> None:
        global _ACTIVE_TRACER, _AUDIT_HOOK_INSTALLED
        if self._installed:
            return
        self._installed = True
        _ACTIVE_TRACER = self

        if hasattr(sys, "monitoring"):
            mon = sys.monitoring
            for candidate in (mon.OPTIMIZER_ID, 4, 3, 2):
                try:
                    mon.use_tool_id(candidate, "pyrn-tia")
                    self.tool_id = candidate
                    break
                except ValueError:
                    continue
            if self.tool_id is not None:
                audit_suffix = "+audit+environ" if self.enable_audit else "+environ"
                self.backend = f"sys.monitoring.PY_START+DISABLE({self.granularity}){audit_suffix}"
                mon.register_callback(self.tool_id, mon.events.PY_START, self._on_py_start)
                mon.set_events(self.tool_id, mon.events.PY_START)

        if self.enable_audit and not _AUDIT_HOOK_INSTALLED:
            sys.addaudithook(_audit_hook)
            _AUDIT_HOOK_INSTALLED = True

        _install_environ_read_proxy()

    def uninstall(self) -> None:
        global _ACTIVE_TRACER
        if not self._installed:
            return
        self._installed = False
        if _ACTIVE_TRACER is self:
            _ACTIVE_TRACER = None
        if self.tool_id is not None and hasattr(sys, "monitoring"):
            mon = sys.monitoring
            with contextlib.suppress(Exception):
                mon.set_events(self.tool_id, 0)
                mon.register_callback(self.tool_id, mon.events.PY_START, None)
                mon.free_tool_id(self.tool_id)
            self.tool_id = None

    def _on_py_start(self, code: Any, instruction_offset: int) -> Any:
        del instruction_offset
        cap = self.active
        if cap is None:
            return sys.monitoring.DISABLE
        rel = self._rel_cached(code.co_filename)
        if rel is not None:
            cap.executed_py_files.add(rel)
            if self.granularity == "file":
                self.file_py_accum.add(rel)
            if self.capture_symbols:
                sym = f"{rel}::{code.co_qualname}"
                cap.executed_symbols.add(sym)
                if self.granularity == "file":
                    self.file_sym_accum.add(sym)
        return sys.monitoring.DISABLE

    def begin_test(self, item: pytest.Item) -> None:
        rel_file = _normalize_repo_rel(item.path, self.root_str, self.root_lower) or str(item.path)
        markers = sorted({m.name for m in item.iter_markers()})
        fixtures = sorted(set(getattr(item, "fixturenames", ())))
        taints: set[str] = set()
        for m in markers:
            if m in _TAINTED_MARKERS:
                taints.add(f"marker:{m}")
        for f in fixtures:
            if f in _TAINTED_FIXTURES:
                taints.add(f"fixture:{f}")

        should_restart = self.granularity == "test" or rel_file != self.last_file_path
        if rel_file != self.last_file_path:
            self.last_file_path = rel_file
            self.file_py_accum.clear()
            self.file_sym_accum.clear()

        self.active = _ActiveTestCapture(
            nodeid=item.nodeid,
            file_path=rel_file,
            fixtures=fixtures,
            markers=markers,
            taint_reasons=taints,
            start_ns=time.perf_counter_ns(),
        )

        if should_restart and self.tool_id is not None and hasattr(sys, "monitoring"):
            sys.monitoring.restart_events()
            self.restart_count += 1

    def end_test(self, outcome: str) -> None:
        cap = self.active
        self.active = None
        if cap is None:
            return
        cap.duration_ms = (time.perf_counter_ns() - cap.start_ns) / 1_000_000.0
        cap.outcome = outcome
        exec_py = (
            sorted(self.file_py_accum | cap.executed_py_files)
            if self.granularity == "file"
            else sorted(cap.executed_py_files)
        )
        exec_sym = (
            sorted(self.file_sym_accum | cap.executed_symbols)
            if self.granularity == "file"
            else sorted(cap.executed_symbols)
        )
        self.records.append(
            {
                "nodeid": cap.nodeid,
                "file_path": cap.file_path,
                "outcome": cap.outcome,
                "duration_ms": round(cap.duration_ms, 3),
                "executed_py_files": exec_py,
                "executed_symbols": exec_sym,
                "read_assets": sorted(cap.read_assets),
                "listed_dirs": sorted(cap.listed_dirs),
                "env_vars_read": cap.env_vars_read,
                "fixtures": cap.fixtures,
                "markers": cap.markers,
                "taint_reasons": sorted(cap.taint_reasons),
            }
        )

    def flush(self) -> None:
        total_wall_ms = (time.perf_counter_ns() - self.session_start_ns) / 1_000_000.0
        payload = {
            "schema_version": 1,
            "python_version": f"{sys.version_info.major}.{sys.version_info.minor}.{sys.version_info.micro}",
            "monitoring_backend": self.backend,
            "restart_events_calls": self.restart_count,
            "total_wall_ms": round(total_wall_ms, 2),
            "tests": self.records,
        }
        self.out_path.parent.mkdir(parents=True, exist_ok=True)
        self.out_path.write_text(json.dumps(payload, indent=2), encoding="utf-8")


_TRACER_KEY: pytest.StashKey[_PyrnTracer] = pytest.StashKey()
_OUTCOME_KEY: pytest.StashKey[list[str]] = pytest.StashKey()


def pytest_addoption(parser: pytest.Parser) -> None:
    group = parser.getgroup("pyrn TIA tracer")
    group.addoption(
        "--pyrn-trace-out",
        default=None,
        help="Write PEP 669 sys.monitoring dynamic TIA trace JSON to this path",
    )
    group.addoption(
        "--pyrn-granularity",
        choices=("file", "test"),
        default="file",
        help="Restart sys.monitoring one-shot events per test 'file' (low overhead) or per 'test'",
    )
    group.addoption(
        "--pyrn-trace-symbols",
        action="store_true",
        default=False,
        help="Also record per-code-object qualnames",
    )
    group.addoption(
        "--pyrn-no-audit",
        action="store_true",
        default=False,
        help="Disable PEP 578 sys.addaudithook",
    )


def pytest_configure(config: pytest.Config) -> None:
    out = config.getoption("pyrn_trace_out")
    if not out:
        return
    tracer = _PyrnTracer(
        root=Path(str(config.rootpath)),
        out_path=Path(out),
        granularity=str(config.getoption("pyrn_granularity")),
        capture_symbols=bool(config.getoption("pyrn_trace_symbols")),
        enable_audit=not bool(config.getoption("pyrn_no_audit")),
    )
    tracer.install()
    config.stash[_TRACER_KEY] = tracer


def pytest_unconfigure(config: pytest.Config) -> None:
    tracer = config.stash.get(_TRACER_KEY, None)
    if tracer is not None:
        tracer.uninstall()
        tracer.flush()


@pytest.hookimpl(hookwrapper=True)
def pytest_runtest_protocol(item: pytest.Item, nextitem: pytest.Item | None) -> Any:
    del nextitem
    tracer = item.config.stash.get(_TRACER_KEY, None)
    if tracer is None:
        yield
        return
    tracer.begin_test(item)
    outcome_holder = ["passed"]
    item.stash[_OUTCOME_KEY] = outcome_holder
    try:
        yield
    finally:
        tracer.end_test(outcome_holder[0])


@pytest.hookimpl(hookwrapper=True)
def pytest_runtest_makereport(item: pytest.Item, call: pytest.CallInfo[None]) -> Any:
    del call
    outcome = yield
    report: pytest.TestReport = outcome.get_result()
    holder = item.stash.get(_OUTCOME_KEY, None)
    if holder is not None:
        if report.failed:
            holder[0] = "failed"
        elif report.skipped and holder[0] != "failed":
            holder[0] = "skipped"
