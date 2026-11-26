"""Launch the Rust ``pyrn`` executable bundled with this zero-dependency Python wheel."""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path


def find_pyrn_bin() -> Path:
    """Return the path to the bundled ``pyrn`` native executable."""
    import pyrn

    binary_name = "pyrn.exe" if os.name == "nt" else "pyrn"
    candidates: list[Path] = []
    override = os.environ.get("PYRN_BIN")
    if override:
        candidates.append(Path(override))
    for entry in getattr(pyrn, "__path__", ()):
        candidates.append(Path(entry) / "_bin" / binary_name)
    candidates.append(Path(__file__).resolve().parent / "_bin" / binary_name)

    seen: set[Path] = set()
    for candidate in candidates:
        resolved = candidate.resolve()
        if resolved in seen:
            continue
        seen.add(resolved)
        if candidate.is_file():
            return candidate
    raise FileNotFoundError(
        "pyrn installation is missing its native executable. Reinstall the pyrn platform wheel."
    )


def main(argv: list[str] | None = None) -> int:
    """Console entry point for the ``pyrn`` script and ``python -m pyrn``."""
    args = sys.argv[1:] if argv is None else list(argv)
    return _launch(args, console=argv is None)


def run_native(args: list[str]) -> int:
    """Invoke the bundled ``pyrn`` executable and return its exit code."""
    return _launch(list(args), console=False)


def _launch(args: list[str], *, console: bool) -> int:
    try:
        executable = find_pyrn_bin()
    except FileNotFoundError as error:
        print(str(error), file=sys.stderr)
        return 2

    if console and os.name == "posix":
        os.execve(executable, [str(executable), *args], os.environ)
    return _call(executable, args, dict(os.environ))


def _call(executable: Path, args: list[str], environment: dict[str, str]) -> int:
    try:
        code = subprocess.call([str(executable), *args], env=environment)
    except KeyboardInterrupt:
        return 130
    return 128 - code if code < 0 else code
