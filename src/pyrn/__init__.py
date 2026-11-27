"""Fast Rust-powered Python/pytest TIA, CAS cache, and isolated warm execution engine."""

from __future__ import annotations

from pyrn.entrypoint import find_pyrn_bin, main, run_native

__version__ = "0.2.1"
__all__ = ["__version__", "find_pyrn_bin", "main", "run_native"]
