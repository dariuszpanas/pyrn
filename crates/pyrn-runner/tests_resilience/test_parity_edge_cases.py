"""Adversarial parity regression tests for Hybrid Direct Fast-Path, opt-in
`pure_autouse_guards`, and `os.environ` dynamic tracing.
"""

from __future__ import annotations

import os
import pytest


@pytest.fixture
def doubled(request: pytest.FixtureRequest) -> int:
    return int(request.param) * 2


@pytest.mark.parametrize("doubled", [5, 7], indirect=True)
def test_indirect_parametrize_fixture_is_never_bypassed(doubled: int) -> None:
    assert doubled in (10, 14), f"Fixture bypassed! got {doubled}"


@pytest.mark.parametrize("x,expected", [(2, 4), (3, 9), (4, 16)])
def test_pure_direct_parametrize_fast_path(x: int, expected: int) -> None:
    assert x * x == expected


def test_reads_feature_env_var() -> None:
    val = os.environ.get("PYRN_TEST_ENV_PROBE", "default")
    absent = os.getenv("PYRN_TEST_ENV_ABSENT")
    assert val == "default" or len(val) > 0
    assert absent is None or len(absent) >= 0


@pytest.fixture(autouse=True)
def _zero_arg_cleanup_probe() -> None:
    if os.environ.get("PYRN_RESILIENCE_FAIL_AUTOUSE") == "1":
        raise RuntimeError("simulated zero-arg autouse failure")
