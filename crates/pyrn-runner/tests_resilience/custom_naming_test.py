"""Verifies `pyproject.toml` `python_files = ["test_*.py", "*_test.py"]` discovery."""

from __future__ import annotations


def test_suffix_named_module_is_discovered() -> None:
    assert "pyrn".startswith("py")
