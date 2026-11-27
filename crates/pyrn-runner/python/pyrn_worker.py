#!/usr/bin/env python3
"""Persistent isolated pytest worker controlled by Rust `pyrn-runner` over loopback TCP NDJSON.

Key capabilities:
- Single persistent `pytest.Config` + `pytest.Session` per worker process (amortizes interpreter
  boot, `conftest.py` loading, `django.setup()`, and session fixtures across work units and
  warm daemon runs).
- Incremental per-unit collection via `session.perform_collect(args=...)` with exact pytest 9.x
  parity (markers, fixtures, hooks, parametrization, `unittest.TestCase`, assertion rewriting).
- Session fixture keep-alive across units via `SetupState.teardown_exact(keepalive_sentinel)`.
- BLAKE3-keyed SQLite template snapshot cloning for BOTH:
    1. In-memory `:memory:` Django test DBs (`BaseDatabaseCreation.create_test_db` via
       `sqlite3.Connection.backup()`, plus ~2.5 ms between-unit reset).
    2. File-backed fresh SQLite DBs (`BaseDatabaseCreation.create_test_db` AND fresh empty
       `django.core.management.commands.migrate.Command.handle` calls such as per-test
       owned SQLite fixtures), with strict BLAKE3 `migrations_hash` + runtime settings keying
       and automatic fail-safe fallback.
- Conservative Hybrid Direct Fast-Path for provably pure zero-fixture pytest functions, with
  zero hardcoded repository fixture names and opt-in `.pyrn.toml` `pure_autouse_guards`.
- Optional built-in PEP 669 `sys.monitoring` (`PY_START` + `DISABLE`) + PEP 578
  `sys.addaudithook` + transparent `os._Environ.__getitem__` read proxy emitting
  `pyrn_core::dynamic::DynamicTestTrace` records per test.
- Optional built-in `--cov-sysmon` coverage capture using CPython 3.12 `COVERAGE_CORE=sysmon`
  into per-worker isolated `COVERAGE_FILE` shards (merged once by Rust at run end).
- Per-test `test_start` heartbeats so the Rust supervisor enforces hard watchdogs and exact
  crash/hang attribution with automatic re-queueing of remaining tests.
"""

from __future__ import annotations

import contextlib
import hashlib
import inspect
import io
import json
import os
import shutil
import socket
import sqlite3
import sys
import time
import traceback
import unittest
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any


# Built-in pytest-django / pytest autouse guards that are strict no-ops for pure tests in modules
# that do not import/touch Django mail, sites, urlconf, or templates.
# NOTE: Module origin (`pytest_django.*` / `_pytest.*`) is ALWAYS verified at runtime so a
# user-defined fixture in any repo's conftest.py can NEVER be elided by name collision.
_ELIDABLE_BUILTIN_AUTOUSE = frozenset(
    {
        "_dj_autoclear_mailbox",
        "_django_clear_site_cache",
        "_django_db_marker",
        "_django_isolate_apps",
        "_django_set_urlconf",
        "_django_setup_unittest",
        "_fail_for_invalid_template_variable",
        "_live_server_helper",
        "_template_string_if_invalid_marker",
        "django_db_blocker",
        "django_test_environment",
        "monkeypatch",
        "request",
    }
)

_UNSAFE_FAST_PATH_MODULE_TOKENS = (
    "django.core.mail",
    "override_settings",
    "modify_settings",
    "django.contrib.sites",
    "set_urlconf",
    "django.template",
)

_TAINTED_MARKERS = frozenset(
    {"real_ray", "live_cluster", "postgresql", "compiled_graph_opt_in"}
)
_TAINTED_FIXTURES = frozenset({"ray_cluster", "live_ray_cluster", "live_server"})
_AUDIT_EVENTS = frozenset(
    {"open", "os.listdir", "os.scandir", "subprocess.Popen", "socket.connect"}
)

# Ephemeral per-worker isolation env vars MUST be excluded from `env_vars_read` so two workers
# with distinct sandbox temp dirs/port ranges produce identical CAS cache keys.
_IGNORED_TRACE_ENV_KEYS = frozenset(
    {
        "TMP",
        "TEMP",
        "TMPDIR",
        "PYTEST_CURRENT_TEST",
        "PYRN_WORKER_ID",
        "PYRN_PORT_START",
        "PYRN_PORT_END",
        "RUNNER_PROTO_WORKER_ID",
        "RUNNER_PROTO_PORT_START",
        "RUNNER_PROTO_PORT_END",
        "DJANGO_LIVE_TEST_SERVER_ADDRESS",
        "COVERAGE_FILE",
        "COVERAGE_CORE",
        "COV_CORE_SOURCE",
        "COV_CORE_CONFIG",
        "COV_CORE_DATAFILE",
    }
)


class _JsonChannel:
    """Line-delimited JSON channel over a dedicated loopback TCP socket."""

    def __init__(self, host: str, port: int) -> None:
        self._sock = socket.create_connection((host, port), timeout=30.0)
        self._sock.settimeout(None)
        try:
            self._sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        except OSError:
            pass
        self._rfile = self._sock.makefile("r", encoding="utf-8", newline="\n")
        self._wfile = self._sock.makefile("w", encoding="utf-8", newline="\n")

    def send(self, payload: dict[str, Any]) -> None:
        line = json.dumps(payload, separators=(",", ":"), ensure_ascii=False)
        self._wfile.write(line + "\n")
        self._wfile.flush()

    def recv(self) -> dict[str, Any] | None:
        line = self._rfile.readline()
        if not line:
            return None
        return json.loads(line)

    def close(self) -> None:
        for handle in (self._wfile, self._rfile, self._sock):
            try:
                handle.close()
            except OSError:
                pass


# ---------------------------------------------------------------------------
# Dynamic PEP 669 + PEP 578 + transparent os.environ read-proxy tracer
# ---------------------------------------------------------------------------

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


_ACTIVE_TRACER: _WorkerTracer | None = None
_AUDIT_HOOK_INSTALLED = False
_ENVIRON_HOOK_INSTALLED = False
_ORIG_ENVIRON_GETITEM: Any = None


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
    """Wrap `os._Environ.__getitem__` in place without replacing `os.environ`.

    Preserves `id(os.environ)`, `isinstance`, `monkeypatch.setenv`, and `mock.patch.dict`
    while transparently capturing `os.environ[k]`, `os.environ.get(k)`, `os.getenv(k)`,
    and `k in os.environ` (all of which delegate to `__getitem__` on `os._Environ`).
    """
    global _ENVIRON_HOOK_INSTALLED, _ORIG_ENVIRON_GETITEM
    if _ENVIRON_HOOK_INSTALLED:
        return
    environ_cls = type(os.environ)
    orig = environ_cls.__getitem__
    _ORIG_ENVIRON_GETITEM = orig

    def _traced_getitem(self: Any, key: Any) -> Any:
        tracer = _ACTIVE_TRACER
        if tracer is None or self is not os.environ:
            return orig(self, key)
        cap = tracer.active
        if cap is None:
            return orig(self, key)

        if isinstance(key, str):
            norm_key = key.upper() if sys.platform == "win32" else key
        else:
            norm_key = None

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


class _WorkerTracer:
    """Zero-monkeypatch PEP 669 + PEP 578 + os.environ dynamic tracer for `pyrn_worker.py`."""

    def __init__(
        self,
        root: Path,
        granularity: str = "file",
        capture_symbols: bool = False,
    ) -> None:
        self.root = root.resolve()
        root_s = str(self.root)
        if not root_s.endswith(os.sep):
            root_s = root_s + os.sep
        self.root_str = root_s
        self.root_lower = root_s.lower()
        self.granularity = granularity
        self.capture_symbols = capture_symbols
        self.active: _ActiveTestCapture | None = None
        self.last_file_path: str | None = None
        self.file_py_accum: set[str] = set()
        self.file_sym_accum: set[str] = set()
        self.tool_id: int | None = None
        self.backend = "none"
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
            _ACTIVE_TRACER = self
            return
        self._installed = True
        _ACTIVE_TRACER = self

        # Prefer OPTIMIZER_ID (5) and 4, 3, 2 — avoid COVERAGE_ID (1) so `--cov-sysmon`
        # can run concurrently without sys.monitoring tool ID collision!
        if hasattr(sys, "monitoring"):
            mon = sys.monitoring
            for candidate in (mon.OPTIMIZER_ID, 4, 3, 2):
                try:
                    mon.use_tool_id(candidate, "pyrn-worker-tia")
                    self.tool_id = candidate
                    break
                except ValueError:
                    continue
            if self.tool_id is not None:
                self.backend = f"sys.monitoring.PY_START+DISABLE({self.granularity})+audit+environ"
                mon.register_callback(self.tool_id, mon.events.PY_START, self._on_py_start)
                mon.set_events(self.tool_id, mon.events.PY_START)

        if not _AUDIT_HOOK_INSTALLED:
            sys.addaudithook(_audit_hook)
            _AUDIT_HOOK_INSTALLED = True

        _install_environ_read_proxy()

    def pause(self) -> None:
        global _ACTIVE_TRACER
        self.active = None
        if _ACTIVE_TRACER is self:
            _ACTIVE_TRACER = None

    def resume(self, granularity: str, capture_symbols: bool) -> None:
        global _ACTIVE_TRACER
        self.granularity = granularity
        self.capture_symbols = capture_symbols
        if not self._installed:
            self.install()
        else:
            _ACTIVE_TRACER = self

    def uninstall(self) -> None:
        global _ACTIVE_TRACER
        if not self._installed:
            return
        self._installed = False
        self.active = None
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

    def begin_test(self, item: Any) -> None:
        rel_file = _normalize_repo_rel(getattr(item, "path", ""), self.root_str, self.root_lower)
        if rel_file is None:
            rel_file = str(getattr(item, "path", "")).replace("\\", "/")
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

    def end_test(self, outcome: str) -> dict[str, Any] | None:
        cap = self.active
        self.active = None
        if cap is None:
            return None
        duration_ms = (time.perf_counter_ns() - cap.start_ns) / 1_000_000.0
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
        return {
            "nodeid": cap.nodeid,
            "file_path": cap.file_path,
            "outcome": outcome,
            "duration_ms": round(duration_ms, 3),
            "executed_py_files": exec_py,
            "executed_symbols": exec_sym,
            "read_assets": sorted(cap.read_assets),
            "listed_dirs": sorted(cap.listed_dirs),
            "env_vars_read": cap.env_vars_read,
            "fixtures": cap.fixtures,
            "markers": cap.markers,
            "taint_reasons": sorted(cap.taint_reasons),
        }


# ---------------------------------------------------------------------------
# SQLite template cloning (both `:memory:` AND file-backed fresh SQLite DBs)
# ---------------------------------------------------------------------------

def _snapshot_sqlite_connection(src_conn: sqlite3.Connection, tpl_path: Path) -> None:
    """Atomically snapshot an open SQLite connection into `tpl_path`."""
    tpl_path.parent.mkdir(parents=True, exist_ok=True)
    tmp_tpl = tpl_path.with_name(f"{tpl_path.name}.tmp.{os.getpid()}.{time.time_ns()}")
    dst_conn = sqlite3.connect(str(tmp_tpl))
    try:
        src_conn.backup(dst_conn)
    finally:
        dst_conn.close()
    try:
        os.replace(tmp_tpl, tpl_path)
    except OSError:
        with contextlib.suppress(OSError):
            tmp_tpl.unlink()


def _remove_sqlite_sidecars(db_path: Path) -> None:
    for suffix in ("-wal", "-shm", "-journal"):
        sidecar = Path(f"{db_path}{suffix}")
        if sidecar.exists():
            with contextlib.suppress(OSError):
                sidecar.unlink()


def _is_empty_sqlite_file(db_path: Path, django_conn: Any) -> bool:
    """Return True iff the file-backed SQLite database does not exist yet or has 0 tables."""
    if not db_path.exists():
        return True
    try:
        if db_path.stat().st_size == 0:
            return True
    except OSError:
        return False
    # File exists and is non-zero; verify sqlite_master is completely empty before cloning.
    try:
        if getattr(django_conn, "connection", None) is not None:
            cur = django_conn.connection.execute("SELECT count(*) FROM sqlite_master")
            row = cur.fetchone()
            return bool(row is not None and row[0] == 0)
        probe = sqlite3.connect(f"file:{db_path.as_posix()}?mode=ro", uri=True)
        try:
            row = probe.execute("SELECT count(*) FROM sqlite_master").fetchone()
            return bool(row is not None and row[0] == 0)
        finally:
            probe.close()
    except Exception:
        return False


def _clear_contenttype_cache() -> None:
    with contextlib.suppress(Exception):
        from django.contrib.contenttypes.models import ContentType

        ContentType.objects.clear_cache()


def _install_sqlite_template_hook(template_path: str, stats: dict[str, Any]) -> None:
    """Patch Django's SQLite creation & full-migrate paths for BLAKE3-keyed template cloning.

    Supports:
    1. `:memory:` SQLite via `BaseDatabaseCreation.create_test_db` (`sqlite3.backup()`).
    2. File-backed SQLite test DBs via `BaseDatabaseCreation.create_test_db` (`shutil.copyfile`).
    3. Repeated fresh file-backed `call_command("migrate", database=..., verbosity=0)` calls
       (e.g. per-test owned SQLite fixtures like `test_doctor.py`'s `owned_database`), strictly
       gated to full, non-partial, non-fake, verbosity=0 migrations on empty file-backed DBs
       and keyed by both Rust's BLAKE3 `migrations_hash` AND runtime `(INSTALLED_APPS,
       MIGRATION_MODULES, run_syncdb)` fingerprint.
    """
    if not template_path:
        return
    try:
        from django.conf import settings as dj_settings
        from django.db import connections as dj_connections
        from django.db.backends.base.creation import BaseDatabaseCreation
    except Exception:
        return

    if getattr(BaseDatabaseCreation, "_pyrn_patched", False):
        return

    in_template_build = [False]
    original_create_test_db = BaseDatabaseCreation.create_test_db

    def _patched_create_test_db(
        self: Any,
        verbosity: int = 1,
        autoclobber: bool = False,
        serialize: Any = None,
        keepdb: bool = False,
    ) -> str:
        t0 = time.perf_counter()
        if self.connection.vendor != "sqlite":
            return original_create_test_db(
                self,
                verbosity=verbosity,
                autoclobber=autoclobber,
                serialize=serialize,
                keepdb=keepdb,
            )

        test_database_name = self._get_test_db_name()
        is_mem = self.is_in_memory_db(test_database_name)
        is_plain_file = (
            not is_mem
            and bool(test_database_name)
            and not str(test_database_name).startswith("file:")
        )
        if not is_mem and not is_plain_file:
            return original_create_test_db(
                self,
                verbosity=verbosity,
                autoclobber=autoclobber,
                serialize=serialize,
                keepdb=keepdb,
            )

        tpl = Path(template_path)
        if tpl.is_file() and tpl.stat().st_size > 0 and not keepdb:
            try:
                if is_mem:
                    self._create_test_db(verbosity, autoclobber, keepdb)
                    self.connection.close()
                    dj_settings.DATABASES[self.connection.alias]["NAME"] = test_database_name
                    self.connection.settings_dict["NAME"] = test_database_name
                    self.connection.ensure_connection()

                    src_conn = sqlite3.connect(f"file:{tpl.as_posix()}?mode=ro", uri=True)
                    try:
                        src_conn.backup(self.connection.connection)
                    finally:
                        src_conn.close()
                else:
                    target_path = Path(str(test_database_name))
                    self.connection.close()
                    target_path.parent.mkdir(parents=True, exist_ok=True)
                    _remove_sqlite_sidecars(target_path)
                    shutil.copyfile(tpl, target_path)
                    dj_settings.DATABASES[self.connection.alias]["NAME"] = test_database_name
                    self.connection.settings_dict["NAME"] = test_database_name
                    self.connection.ensure_connection()

                _clear_contenttype_cache()
                if serialize:
                    self.connection._test_serialized_contents = self.serialize_db_to_string()

                stats["db_setup_ms"] += (time.perf_counter() - t0) * 1000.0
                stats["db_template_hit"] = True
                stats["db_template_hits"] += 1
                return test_database_name
            except Exception:
                # Fail safe: fall back to full original migration creation.
                pass

        in_template_build[0] = True
        try:
            result = original_create_test_db(
                self,
                verbosity=verbosity,
                autoclobber=autoclobber,
                serialize=serialize,
                keepdb=keepdb,
            )
        finally:
            in_template_build[0] = False

        stats["db_setup_ms"] += (time.perf_counter() - t0) * 1000.0
        stats["db_template_hit"] = False

        # Save snapshot atomically for subsequent workers and future runs.
        try:
            self.connection.ensure_connection()
            _snapshot_sqlite_connection(self.connection.connection, tpl)
        except Exception:
            pass

        return result

    BaseDatabaseCreation.create_test_db = _patched_create_test_db  # type: ignore[method-assign]
    BaseDatabaseCreation._pyrn_patched = True  # type: ignore[attr-defined]

    # Hook 2: Fresh empty file-backed `call_command("migrate", ...)` cloning.
    try:
        from django.core.management.commands.migrate import Command as MigrateCommand
    except Exception:
        return

    if getattr(MigrateCommand, "_pyrn_patched", False):
        return

    original_migrate_handle = MigrateCommand.handle
    base_tpl = Path(template_path)

    def _patched_migrate_handle(self: Any, *args: Any, **options: Any) -> Any:
        if in_template_build[0]:
            return original_migrate_handle(self, *args, **options)

        # Only accelerate full, silent, standard migrations with zero custom targeting/flags.
        if (
            options.get("app_label") is not None
            or options.get("migration_name") is not None
            or options.get("fake")
            or options.get("fake_initial")
            or options.get("plan")
            or options.get("check_unapplied")
            or options.get("prune")
            or int(options.get("verbosity", 1)) != 0
        ):
            return original_migrate_handle(self, *args, **options)

        alias = options.get("database") or "default"
        try:
            conn = dj_connections[alias]
        except Exception:
            return original_migrate_handle(self, *args, **options)

        if getattr(conn, "vendor", None) != "sqlite":
            return original_migrate_handle(self, *args, **options)

        db_name = str(conn.settings_dict.get("NAME") or "")
        if not db_name or db_name == ":memory:" or db_name.startswith("file:"):
            return original_migrate_handle(self, *args, **options)

        db_path = Path(db_name)
        if not _is_empty_sqlite_file(db_path, conn):
            return original_migrate_handle(self, *args, **options)

        # Key strictly by Rust's migrations_hash (in base_tpl.stem) + runtime Django settings.
        try:
            installed = tuple(str(a) for a in getattr(dj_settings, "INSTALLED_APPS", ()))
            mig_mods = getattr(dj_settings, "MIGRATION_MODULES", None) or {}
            mig_mods_sig = tuple(sorted((str(k), str(v)) for k, v in mig_mods.items()))
            run_syncdb = bool(options.get("run_syncdb", False))
            sig_bytes = repr((installed, mig_mods_sig, run_syncdb)).encode("utf-8")
            settings_sig = hashlib.blake2b(sig_bytes, digest_size=8).hexdigest()
        except Exception:
            return original_migrate_handle(self, *args, **options)

        tpl_migrate = base_tpl.with_name(f"{base_tpl.stem}_file_{settings_sig}.sqlite3")

        t0 = time.perf_counter()
        if tpl_migrate.is_file() and tpl_migrate.stat().st_size > 0:
            try:
                conn.close()
                db_path.parent.mkdir(parents=True, exist_ok=True)
                _remove_sqlite_sidecars(db_path)
                shutil.copyfile(tpl_migrate, db_path)
                _clear_contenttype_cache()
                stats["db_setup_ms"] += (time.perf_counter() - t0) * 1000.0
                stats["db_template_hit"] = True
                stats["db_template_hits"] += 1
                return None
            except Exception:
                # Fail safe: run original migration cleanly.
                pass

        in_template_build[0] = True
        try:
            res = original_migrate_handle(self, *args, **options)
        finally:
            in_template_build[0] = False

        stats["db_setup_ms"] += (time.perf_counter() - t0) * 1000.0

        # Atomically snapshot the freshly migrated file-backed SQLite DB.
        try:
            if getattr(conn, "connection", None) is not None:
                _snapshot_sqlite_connection(conn.connection, tpl_migrate)
            elif db_path.is_file():
                src_conn = sqlite3.connect(str(db_path))
                try:
                    _snapshot_sqlite_connection(src_conn, tpl_migrate)
                finally:
                    src_conn.close()
        except Exception:
            pass

        return res

    MigrateCommand.handle = _patched_migrate_handle  # type: ignore[method-assign]
    MigrateCommand._pyrn_patched = True  # type: ignore[attr-defined]


def _reset_sqlite_from_template_if_active(template_path: str) -> None:
    """Restore an already-open Django :memory: SQLite test DB from the pristine template (~2-3ms)."""
    tpl = Path(template_path)
    if not tpl.is_file() or tpl.stat().st_size == 0:
        return
    try:
        from django.db import connections

        conn = connections["default"]
        if conn.vendor != "sqlite":
            return
        db_name = conn.settings_dict.get("NAME", "")
        if not conn.creation.is_in_memory_db(db_name):
            return
        if conn.connection is None:
            return
        with contextlib.suppress(Exception):
            conn.connection.rollback()
        src_conn = sqlite3.connect(f"file:{tpl.as_posix()}?mode=ro", uri=True)
        try:
            src_conn.backup(conn.connection)
        finally:
            src_conn.close()
        _clear_contenttype_cache()
    except Exception:
        pass


# ---------------------------------------------------------------------------
# Pytest session helpers, Hybrid Direct Fast-Path, and Sharding
# ---------------------------------------------------------------------------

class _WorkerPlugin:
    """Lightweight pytest plugin to track deselected items and collection errors."""

    def __init__(self) -> None:
        self.deselected_count: int = 0
        self.collection_errors: list[str] = []

    def reset_unit(self) -> None:
        self.deselected_count = 0
        self.collection_errors = []

    def pytest_deselected(self, items: list[Any]) -> None:
        self.deselected_count += len(items)

    def pytest_collectreport(self, report: Any) -> None:
        if report.failed:
            self.collection_errors.append(str(report.longrepr))


def _make_keepalive_item(session: Any) -> Any:
    """Create a Session-child sentinel item so teardown_exact preserves session fixtures."""
    import pytest

    class _KeepAliveItem(pytest.Item):
        def runtest(self) -> None:
            pass

    return _KeepAliveItem.from_parent(session, name="__pyrn_session_keepalive__")


def _module_source_safe_for_fast_path(mod_path: Path, cache: dict[Path, bool]) -> bool:
    cached = cache.get(mod_path)
    if cached is not None:
        return cached
    try:
        src = mod_path.read_text(encoding="utf-8", errors="ignore")
    except OSError:
        cache[mod_path] = False
        return False
    safe = not any(tok in src for tok in _UNSAFE_FAST_PATH_MODULE_TOKENS)
    cache[mod_path] = safe
    return safe


def _check_fast_path(
    item: Any,
    module_safe_cache: dict[Path, bool],
    pure_autouse_guards: frozenset[str],
) -> tuple[bool, list[Any]]:
    """Return (eligible, zero_arg_cleanup_funcs) with strict conservative parity checks."""
    if type(item).__name__ != "Function":
        return False, []

    cls = getattr(item, "cls", None)
    if cls is not None:
        if issubclass(cls, unittest.TestCase):
            return False, []
        for hook_name in (
            "setup_method",
            "teardown_method",
            "setup_class",
            "teardown_class",
            "setup",
            "teardown",
        ):
            if hasattr(cls, hook_name):
                return False, []

    mod = getattr(item, "module", None)
    if mod is not None:
        for hook_name in (
            "setup_module",
            "teardown_module",
            "setup_function",
            "teardown_function",
        ):
            if hasattr(mod, hook_name):
                return False, []

    item_path = Path(str(getattr(item, "path", "")))
    if not _module_source_safe_for_fast_path(item_path, module_safe_cache):
        return False, []

    for marker in item.iter_markers():
        if marker.name != "parametrize":
            return False, []

    callspec = getattr(item, "callspec", None)
    param_names = set(callspec.params.keys()) if callspec is not None else set()

    fixtureinfo = getattr(item, "_fixtureinfo", None)
    if fixtureinfo is None:
        return False, []
    argnames = set(getattr(fixtureinfo, "argnames", ()) or ())
    if not argnames.issubset(param_names):
        return False, []

    func_obj = getattr(item, "obj", None)
    if func_obj is None or inspect.iscoroutinefunction(func_obj):
        return False, []

    # Inspect ALL fixtures (requested args, indirect parametrize targets, and autouse).
    # CRITICAL: check name2defs BEFORE skipping param_names so `@pytest.mark.parametrize(..., indirect=True)`
    # is NEVER misclassified as a direct parameter!
    name2defs = getattr(fixtureinfo, "name2fixturedefs", {}) or {}
    cleanup_funcs: list[Any] = []
    seen_fnames: set[str] = set()
    for fname in tuple(getattr(item, "fixturenames", ())) + tuple(param_names):
        if fname in seen_fnames:
            continue
        seen_fnames.add(fname)
        if fname == "request":
            continue

        defs = name2defs.get(fname)
        if defs:
            fdef = defs[-1]
            fn = getattr(fdef, "func", None)
            if fn is None:
                return False, []
            # Direct (non-indirect) parametrize value provider -> safe!
            if getattr(fn, "__name__", "") == "get_direct_param_fixture_func":
                continue
            # If this fixture name is also in param_names, it is an INDIRECT parametrized fixture!
            if fname in param_names:
                return False, []
            # Built-in pytest-django / _pytest no-op autouse guards (verified by owning module).
            fn_mod = getattr(fn, "__module__", "") or ""
            if fname in _ELIDABLE_BUILTIN_AUTOUSE and (
                fn_mod.startswith("pytest_django.") or fn_mod.startswith("_pytest.")
            ):
                continue
            # Opt-in project `.pyrn.toml` `pure_autouse_guards` (strictly autouse, not explicitly requested).
            if fname in pure_autouse_guards and fname not in argnames:
                continue
            # Zero-argument, non-generator, function-scoped autouse cleanup functions.
            if (
                fname not in argnames
                and not getattr(fdef, "argnames", ())
                and not inspect.isgeneratorfunction(fn)
                and not inspect.iscoroutinefunction(fn)
                and getattr(fdef, "scope", "function") == "function"
            ):
                cleanup_funcs.append(fn)
                continue
            return False, []
        else:
            if fname in param_names:
                continue
            return False, []

    return True, cleanup_funcs


def _run_direct_fast_path(
    item: Any,
    cleanup_funcs: list[Any],
    session: Any,
    keepalive_sentinel: Any,
) -> dict[str, Any]:
    """Execute a provably pure test directly without pytest per-item hook/fixture overhead."""
    import pytest

    if len(session._setupstate.stack) > 1:
        session._setupstate.teardown_exact(keepalive_sentinel)

    t_setup0 = time.perf_counter()
    try:
        for fn in cleanup_funcs:
            fn()
        cls = getattr(item, "cls", None)
        if cls is not None:
            instance = cls()
            raw_func = getattr(item, "_obj", None) or getattr(cls, item.originalname)
            callable_obj = raw_func.__get__(instance, cls)
        else:
            callable_obj = item.obj
    except pytest.skip.Exception as exc:
        setup_ms = (time.perf_counter() - t_setup0) * 1000.0
        return {
            "outcome": "skipped",
            "setup_ms": round(setup_ms, 3),
            "call_ms": 0.0,
            "teardown_ms": 0.0,
            "fast_path": True,
            "longrepr": str(exc) if str(exc) else None,
        }
    except pytest.xfail.Exception as exc:
        setup_ms = (time.perf_counter() - t_setup0) * 1000.0
        return {
            "outcome": "xfailed",
            "setup_ms": round(setup_ms, 3),
            "call_ms": 0.0,
            "teardown_ms": 0.0,
            "fast_path": True,
            "longrepr": str(exc) if str(exc) else None,
        }
    except BaseException as exc:
        if isinstance(exc, KeyboardInterrupt):
            raise
        setup_ms = (time.perf_counter() - t_setup0) * 1000.0
        return {
            "outcome": "error",
            "setup_ms": round(setup_ms, 3),
            "call_ms": 0.0,
            "teardown_ms": 0.0,
            "fast_path": True,
            "longrepr": traceback.format_exc().rstrip(),
        }

    callspec = getattr(item, "callspec", None)
    kwargs = dict(callspec.params) if callspec is not None else {}
    setup_ms = (time.perf_counter() - t_setup0) * 1000.0

    buf_out = io.StringIO()
    buf_err = io.StringIO()
    outcome = "passed"
    longrepr: str | None = None

    t_call0 = time.perf_counter()
    try:
        with contextlib.redirect_stdout(buf_out), contextlib.redirect_stderr(buf_err):
            callable_obj(**kwargs)
    except pytest.skip.Exception as exc:
        outcome = "skipped"
        longrepr = str(exc) if str(exc) else None
    except pytest.xfail.Exception as exc:
        outcome = "xfailed"
        longrepr = str(exc) if str(exc) else None
    except BaseException as exc:
        if isinstance(exc, KeyboardInterrupt):
            raise
        outcome = "failed"
        tb = traceback.format_exc()
        out_s = buf_out.getvalue()
        err_s = buf_err.getvalue()
        parts = [tb.rstrip()]
        if out_s:
            parts.append(f"--- Captured stdout ---\n{out_s.rstrip()}")
        if err_s:
            parts.append(f"--- Captured stderr ---\n{err_s.rstrip()}")
        longrepr = "\n".join(parts)
    call_ms = (time.perf_counter() - t_call0) * 1000.0

    return {
        "outcome": outcome,
        "setup_ms": round(setup_ms, 3),
        "call_ms": round(call_ms, 3),
        "teardown_ms": 0.0,
        "fast_path": True,
        "longrepr": longrepr,
    }


def _run_full_pytest_item(item: Any, nextitem: Any) -> dict[str, Any]:
    """Execute one item through pytest's exact native runtestprotocol."""
    from _pytest.runner import runtestprotocol

    reports = runtestprotocol(item, log=True, nextitem=nextitem)
    rep_map = {r.when: r for r in reports}
    setup_r = rep_map.get("setup")
    call_r = rep_map.get("call")
    teardown_r = rep_map.get("teardown")

    setup_ms = (setup_r.duration * 1000.0) if setup_r else 0.0
    call_ms = (call_r.duration * 1000.0) if call_r else 0.0
    teardown_ms = (teardown_r.duration * 1000.0) if teardown_r else 0.0

    outcome = "passed"
    longrepr: str | None = None

    if setup_r and setup_r.skipped:
        if hasattr(setup_r, "wasxfail"):
            outcome = "xfailed"
        else:
            outcome = "skipped"
        longrepr = str(setup_r.longrepr) if setup_r.longrepr else None
    elif setup_r and setup_r.failed:
        outcome = "error"
        longrepr = str(setup_r.longreprtext)
    elif call_r is not None:
        if call_r.failed:
            outcome = "failed"
            longrepr = str(call_r.longreprtext)
        elif call_r.skipped:
            if hasattr(call_r, "wasxfail"):
                outcome = "xfailed"
            else:
                outcome = "skipped"
            longrepr = str(call_r.longrepr) if call_r.longrepr else None
        elif call_r.passed:
            if hasattr(call_r, "wasxfail"):
                outcome = "xpassed"
            else:
                outcome = "passed"

    if teardown_r and teardown_r.failed:
        if outcome == "passed":
            outcome = "error"
        td_txt = str(teardown_r.longreprtext)
        longrepr = f"{longrepr}\n--- teardown ---\n{td_txt}" if longrepr else td_txt

    return {
        "outcome": outcome,
        "setup_ms": round(setup_ms, 3),
        "call_ms": round(call_ms, 3),
        "teardown_ms": round(teardown_ms, 3),
        "fast_path": False,
        "longrepr": longrepr,
    }


def _select_shard(items: list[Any], shard_index: int, shard_total: int) -> list[Any]:
    """Deterministic class-preserving sharding with 100% disjoint full coverage."""
    if shard_total <= 1 or not items:
        return items

    groups: list[list[Any]] = []
    cur_key: Any = object()
    for it in items:
        key = (getattr(it, "module", None), getattr(it, "cls", None))
        if key != cur_key:
            groups.append([it])
            cur_key = key
        else:
            groups[-1].append(it)

    if len(groups) < shard_total:
        if all(getattr(it, "cls", None) is None for it in items):
            return [it for idx, it in enumerate(items) if idx % shard_total == shard_index]
        n = len(items)
        start = (n * shard_index) // shard_total
        end = (n * (shard_index + 1)) // shard_total
        return items[start:end]

    loads = [0] * shard_total
    indexed_groups = sorted(enumerate(groups), key=lambda kv: (-len(kv[1]), kv[0]))
    bucket_group_order: list[list[tuple[int, list[Any]]]] = [[] for _ in range(shard_total)]
    for orig_idx, grp in indexed_groups:
        target = min(range(shard_total), key=lambda b: (loads[b], b))
        bucket_group_order[target].append((orig_idx, grp))
        loads[target] += len(grp)

    selected: list[Any] = []
    for _, grp in sorted(bucket_group_order[shard_index], key=lambda kv: kv[0]):
        selected.extend(grp)
    return selected


def _prepare_fixturemanager_for_unit(session: Any, py_config: Any) -> None:
    """Rebind pytest 9.2+ node-scoped FixtureManager conftest.py fixtures on fresh Directory collectors."""
    fm = getattr(session, "_fixturemanager", None)
    if fm is None:
        return

    node_auto = getattr(fm, "_node_autousenames", None)
    if isinstance(node_auto, dict):
        for node in [n for n in list(node_auto.keys()) if n is not session]:
            node_auto.pop(node, None)

    arg2node2defs = getattr(fm, "_arg2node2fixturedefs", None)
    if isinstance(arg2node2defs, dict):
        for node_map in arg2node2defs.values():
            if isinstance(node_map, dict):
                for node in [n for n in list(node_map.keys()) if n is not session]:
                    node_map.pop(node, None)

    pending = getattr(fm, "_pending_conftests", None)
    conftest_plugins = getattr(py_config.pluginmanager, "_conftest_plugins", None)
    if isinstance(pending, dict) and conftest_plugins:
        rootpath = Path(str(py_config.rootpath)).resolve()
        for cf_mod in conftest_plugins:
            cf_file = getattr(cf_mod, "__file__", None)
            if not cf_file:
                continue
            try:
                cf_dir = Path(str(cf_file)).resolve().parent
            except OSError:
                continue
            if cf_dir != rootpath:
                try:
                    cf_dir.relative_to(rootpath)
                    pending[cf_dir] = cf_mod
                except ValueError:
                    pass


def _invalidate_changed_modules(changed_paths: list[str], project_root: Path, session: Any) -> None:
    """Purge modified or deleted test modules from sys.modules and pytest caches."""
    if not changed_paths:
        return
    normalized_targets = {
        str((project_root / p).resolve()).lower() if not os.path.isabs(p) else str(Path(p).resolve()).lower()
        for p in changed_paths
    }
    to_delete: list[str] = []
    for mod_name, mod in list(sys.modules.items()):
        mod_file = getattr(mod, "__file__", None)
        if not mod_file:
            continue
        try:
            resolved = str(Path(mod_file).resolve()).lower()
        except OSError:
            continue
        if resolved in normalized_targets:
            to_delete.append(mod_name)
    for mod_name in to_delete:
        sys.modules.pop(mod_name, None)

    if hasattr(session, "_collection_cache"):
        session._collection_cache.clear()


def _create_coverage_controller(cov_data_file: str | None) -> Any:
    """Instantiate a CPython 3.12 sys.monitoring coverage controller when `--cov-sysmon` is enabled."""
    if not cov_data_file:
        return None
    try:
        import coverage

        Path(cov_data_file).parent.mkdir(parents=True, exist_ok=True)
        return coverage.Coverage(
            data_file=cov_data_file,
            data_suffix=False,
            config_file=True,
        )
    except Exception:
        return None


def main() -> int:
    t_boot0 = time.perf_counter()
    if len(sys.argv) < 2:
        sys.stderr.write("usage: pyrn_worker.py <config_json>\n")
        return 2

    cfg = json.loads(sys.argv[1])
    host: str = cfg["host"]
    port: int = int(cfg["port"])
    auth_token: str = cfg["auth_token"]
    worker_id: str = cfg["worker_id"]
    lane: str = cfg["lane"]
    project_root = Path(cfg["project_root"]).resolve()
    config_file: str | None = cfg.get("config_file")
    conftest_seed_dir: str | None = cfg.get("conftest_seed_dir")
    initial_markers: str | None = cfg.get("markers")
    sqlite_template_path: str = cfg.get("sqlite_template_path") or ""
    extra_pytest_args: list[str] = list(cfg.get("extra_pytest_args") or [])
    pure_autouse_guards: frozenset[str] = frozenset(cfg.get("pure_autouse_guards") or ())
    boot_cov_data_file: str | None = cfg.get("cov_data_file")
    cov_data_file: str | None = boot_cov_data_file

    os.chdir(project_root)
    if str(project_root) not in sys.path:
        sys.path.insert(0, str(project_root))

    # Start coverage BEFORE importing pytest/Django/conftest or collecting test modules
    # so top-level imports, class/function definitions, and module constants are measured
    # with exact pytest-cov parity under CPython 3.12+ sys.monitoring.
    cov_ctrl = _create_coverage_controller(cov_data_file)
    cov_running = False
    if cov_ctrl is not None:
        with contextlib.suppress(Exception):
            cov_ctrl.start()
            cov_running = True

    db_stats: dict[str, Any] = {
        "db_setup_ms": 0.0,
        "db_template_hit": False,
        "db_template_hits": 0,
    }
    if sqlite_template_path:
        _install_sqlite_template_hook(sqlite_template_path, db_stats)

    import pytest
    from _pytest.config import _prepareconfig
    from _pytest.main import Session

    worker_plugin = _WorkerPlugin()

    pytest_args: list[str] = [
        "-p",
        "no:cacheprovider",
        "-p",
        "no:xdist",
        "-p",
        "no:cov",
        "--rootdir",
        str(project_root),
        "-o",
        "addopts=",
    ]
    worker_tmp = os.environ.get("TMPDIR") or os.environ.get("TEMP")
    if worker_tmp:
        pytest_args.extend(["--basetemp", str(Path(worker_tmp) / "t")])
    if config_file:
        pytest_args.extend(["-c", str(config_file)])
    if initial_markers:
        pytest_args.extend(["-m", initial_markers])
    pytest_args.extend(extra_pytest_args)

    if conftest_seed_dir and Path(conftest_seed_dir).exists():
        pytest_args.append(str(conftest_seed_dir))

    devnull_out = io.StringIO()
    with contextlib.redirect_stdout(devnull_out):
        py_config = _prepareconfig(pytest_args, plugins=[worker_plugin])
        py_config._do_configure()
        session = Session.from_config(py_config)
        py_config.hook.pytest_sessionstart(session=session)
        if conftest_seed_dir and Path(conftest_seed_dir).exists():
            with contextlib.suppress(Exception):
                py_config.pluginmanager._getconftestmodules(Path(conftest_seed_dir))

    keepalive_sentinel = _make_keepalive_item(session)
    module_safe_cache: dict[Path, bool] = {}
    tracer = _WorkerTracer(root=project_root)

    boot_ms = round((time.perf_counter() - t_boot0) * 1000.0, 3)

    chan = _JsonChannel(host, port)
    chan.send(
        {
            "event": "hello",
            "auth_token": auth_token,
            "worker_id": worker_id,
            "pid": os.getpid(),
            "lane": lane,
            "boot_ms": boot_ms,
        }
    )

    try:
        while True:
            msg = chan.recv()
            if msg is None:
                break
            cmd = msg.get("cmd")
            if cmd == "ping":
                chan.send({"event": "pong", "worker_id": worker_id})
                continue
            if cmd == "shutdown":
                break
            if cmd == "invalidate":
                _invalidate_changed_modules(msg.get("changed_paths") or [], project_root, session)
                module_safe_cache.clear()
                chan.send({"event": "invalidated", "worker_id": worker_id})
                continue
            if cmd != "run_unit":
                chan.send({"event": "worker_error", "message": f"unknown cmd: {cmd}"})
                continue

            unit_id: str = msg["unit_id"]
            paths: list[str] = msg["paths"]
            markers: str | None = msg.get("markers")
            keyword: str | None = msg.get("keyword")
            shard_index: int = int(msg.get("shard_index", 0))
            shard_total: int = int(msg.get("shard_total", 1))
            explicit_nodeids: list[str] | None = msg.get("explicit_nodeids")
            exclude_nodeids: list[str] | None = msg.get("exclude_nodeids")
            fast_path_enabled: bool = bool(msg.get("fast_path", True))
            changed_paths: list[str] = msg.get("changed_paths") or []
            trace_enabled: bool = bool(msg.get("trace", False))
            trace_granularity: str = str(msg.get("trace_granularity") or "file")
            trace_symbols: bool = bool(msg.get("trace_symbols", False))
            unit_cov_file: str | None = msg.get("cov_data_file")

            if changed_paths:
                _invalidate_changed_modules(changed_paths, project_root, session)
                module_safe_cache.clear()

            py_config.option.markexpr = markers or ""
            py_config.option.keyword = keyword or ""

            # Ensure coverage is active BEFORE collection so module-import lines are captured.
            effective_cov_file = unit_cov_file or boot_cov_data_file
            active_cov = cov_ctrl if effective_cov_file else None
            if effective_cov_file and (active_cov is None or effective_cov_file != cov_data_file):
                if cov_ctrl is not None and cov_running:
                    with contextlib.suppress(Exception):
                        cov_ctrl.stop()
                        cov_ctrl.save()
                    cov_running = False
                active_cov = _create_coverage_controller(effective_cov_file)
                cov_ctrl = active_cov
                cov_data_file = effective_cov_file
            if active_cov is not None and not cov_running:
                with contextlib.suppress(Exception):
                    active_cov.start()
                    cov_running = True

            worker_plugin.reset_unit()
            _prepare_fixturemanager_for_unit(session, py_config)
            db_before_ms = float(db_stats["db_setup_ms"])
            hits_before = int(db_stats["db_template_hits"])

            t_col0 = time.perf_counter()
            collect_error: str | None = None
            items: list[Any] = []
            try:
                with contextlib.redirect_stdout(io.StringIO()):
                    collected = list(session.perform_collect(args=paths, genitems=True))
                items = collected
            except BaseException as exc:
                if isinstance(exc, KeyboardInterrupt):
                    raise
                collect_error = f"{type(exc).__name__}: {exc}\n{traceback.format_exc()}"
            collect_ms = round((time.perf_counter() - t_col0) * 1000.0, 3)

            if collect_error or worker_plugin.collection_errors:
                err_text = collect_error or "\n".join(worker_plugin.collection_errors)
                chan.send(
                    {
                        "event": "unit_collect_error",
                        "unit_id": unit_id,
                        "collect_ms": collect_ms,
                        "error": err_text,
                    }
                )
                continue

            if exclude_nodeids:
                excluded = set(exclude_nodeids)

                def _is_excluded(nid: str) -> bool:
                    if nid in excluded:
                        return True
                    bracket = nid.find("[")
                    return bracket != -1 and nid[:bracket] in excluded

                items = [it for it in items if not _is_excluded(it.nodeid)]

            if explicit_nodeids is not None:
                wanted = set(explicit_nodeids)

                def _matches_wanted(nid: str) -> bool:
                    if nid in wanted:
                        return True
                    bracket = nid.find("[")
                    return bracket != -1 and nid[:bracket] in wanted

                items = [it for it in items if _matches_wanted(it.nodeid)]
            elif shard_total > 1:
                items = _select_shard(items, shard_index, shard_total)

            reported_deselected = (
                worker_plugin.deselected_count
                if shard_index == 0 and explicit_nodeids is None
                else 0
            )

            chan.send(
                {
                    "event": "unit_collected",
                    "unit_id": unit_id,
                    "nodeids": [it.nodeid for it in items],
                    "deselected": reported_deselected,
                    "collect_ms": collect_ms,
                }
            )

            if trace_enabled:
                tracer.resume(granularity=trace_granularity, capture_symbols=trace_symbols)
            else:
                tracer.pause()

            total_items = len(items)
            try:
                for idx, item in enumerate(items):
                    chan.send(
                        {
                            "event": "test_start",
                            "unit_id": unit_id,
                            "nodeid": item.nodeid,
                        }
                    )

                    eligible, cleanup_funcs = (
                        _check_fast_path(item, module_safe_cache, pure_autouse_guards)
                        if fast_path_enabled
                        else (False, [])
                    )

                    if trace_enabled:
                        tracer.begin_test(item)

                    if eligible:
                        res = _run_direct_fast_path(
                            item, cleanup_funcs, session, keepalive_sentinel
                        )
                    else:
                        nextitem = (
                            items[idx + 1] if idx + 1 < total_items else keepalive_sentinel
                        )
                        res = _run_full_pytest_item(item, nextitem)

                    trace_rec = tracer.end_test(res["outcome"]) if trace_enabled else None

                    payload: dict[str, Any] = {
                        "event": "test_result",
                        "unit_id": unit_id,
                        "nodeid": item.nodeid,
                        "outcome": res["outcome"],
                        "setup_ms": res["setup_ms"],
                        "call_ms": res["call_ms"],
                        "teardown_ms": res["teardown_ms"],
                        "fast_path": res["fast_path"],
                        "longrepr": res["longrepr"],
                    }
                    if trace_rec is not None:
                        payload["trace"] = trace_rec
                    chan.send(payload)
            finally:
                tracer.pause()
                # Tear down module/class/function fixtures while coverage is still active so
                # fixture-teardown paths are measured, while keeping session fixtures warm.
                with contextlib.suppress(Exception):
                    session._setupstate.teardown_exact(keepalive_sentinel)
                if active_cov is not None and cov_running:
                    with contextlib.suppress(Exception):
                        active_cov.stop()
                        active_cov.save()
                    cov_running = False

            if sqlite_template_path:
                _reset_sqlite_from_template_if_active(sqlite_template_path)

            db_delta_ms = round(max(0.0, float(db_stats["db_setup_ms"]) - db_before_ms), 3)
            hits_delta = max(0, int(db_stats["db_template_hits"]) - hits_before)
            chan.send(
                {
                    "event": "unit_done",
                    "unit_id": unit_id,
                    "db_setup_ms": db_delta_ms,
                    "db_template_hit": hits_delta > 0,
                    "db_template_hits": hits_delta,
                }
            )
    finally:
        tracer.uninstall()
        if cov_ctrl is not None and cov_running:
            with contextlib.suppress(Exception):
                cov_ctrl.stop()
                cov_ctrl.save()
        with contextlib.suppress(Exception):
            session._setupstate.teardown_exact(None)
        with contextlib.suppress(Exception):
            py_config.hook.pytest_sessionfinish(session=session, exitstatus=0)
        with contextlib.suppress(Exception):
            py_config._ensure_unconfigure()
        chan.close()

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
