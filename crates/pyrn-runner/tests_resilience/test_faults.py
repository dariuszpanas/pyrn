"""Resilience test fixture: hard hang, hard crash (os._exit), flake detection,
and crash-during-flake-rerun recovery.
"""

from __future__ import annotations

import os
import time
from pathlib import Path


def test_healthy_before_1() -> None:
    assert 1 + 1 == 2


def test_healthy_before_2() -> None:
    assert "rust".upper() == "RUST"


def test_intentional_hang() -> None:
    if os.environ.get("PYRN_RESILIENCE_HANG") == "1":
        time.sleep(60)
    assert True


def test_healthy_after_hang_1() -> None:
    assert sum(range(10)) == 45


def test_healthy_after_hang_2() -> None:
    assert len([1, 2, 3]) == 3


def test_intentional_crash() -> None:
    if os.environ.get("PYRN_RESILIENCE_CRASH") == "1":
        os._exit(139)
    assert True


def test_healthy_after_crash_1() -> None:
    assert sorted([3, 1, 2]) == [1, 2, 3]


def test_healthy_after_crash_2() -> None:
    assert {"a": 1}["a"] == 1


def test_intentional_flake() -> None:
    marker = os.environ.get("PYRN_RESILIENCE_FLAKE_FILE")
    if marker:
        p = Path(marker)
        if not p.exists():
            p.write_text("seen-once", encoding="utf-8")
            raise AssertionError("simulated transient flake on attempt 1")
    assert True


def test_crash_during_flake_rerun() -> None:
    if os.environ.get("PYRN_RESILIENCE_CRASH_RERUN") == "1":
        os._exit(139)
    assert True
