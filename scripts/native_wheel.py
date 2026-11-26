"""Bundle the standalone Rust ``pyrn`` binary inside a zero-dependency Python platform wheel."""

from __future__ import annotations

import json
import os
import platform as runtime_platform
import shutil
import subprocess
import sysconfig
from pathlib import Path
from typing import Any

from hatchling.builders.hooks.plugin.interface import BuildHookInterface


def wheel_platform(host: str, platform: str, machine: str) -> str:
    platform = platform.replace("-", "_").replace(".", "_")
    if platform.startswith("macosx_") and platform.endswith("_universal2"):
        _, major, minor, _ = platform.split("_")
        if machine == "arm64" and int(major) < 11:
            major, minor = "11", "0"
        platform = f"macosx_{major}_{minor}_{machine}"
    arch = host.split("-", 1)[0]
    architectures = {
        "x86_64": ("_x86_64", "_amd64"),
        "aarch64": ("_aarch64", "_arm64"),
        "i686": ("_i386", "_i686", "win32"),
        "powerpc64": ("_ppc64",),
        "powerpc64le": ("_ppc64le",),
        "armv7": ("_armv7l",),
    }
    if not platform.lower().endswith(architectures.get(arch, (f"_{arch}",))):
        raise RuntimeError(f"Rust host {host} cannot build the interpreter wheel tag {platform}.")
    return platform


class NativeWheelHook(BuildHookInterface):
    def initialize(self, version: str, build_data: dict[str, Any]) -> None:
        if version == "editable":
            raise RuntimeError("Use the project's PEP 660 backend for an editable native wheel.")
        self._build_native_artifacts(build_data)

    def _build_native_artifacts(self, build_data: dict[str, Any]) -> None:
        prebuilt = os.environ.get("PYRN_PREBUILT_BINARY")
        if prebuilt:
            executable = Path(prebuilt).resolve(strict=True)
            tag_platform = os.environ.get("PYRN_WHEEL_PLATFORM")
            if not tag_platform:
                if not shutil.which("rustc"):
                    raise RuntimeError(
                        "Determining the wheel platform tag without PYRN_WHEEL_PLATFORM requires rustc."
                    )
                host = subprocess.check_output(
                    ["rustc", "--print", "host-tuple"], text=True
                ).strip()
                tag_platform = wheel_platform(
                    host, sysconfig.get_platform(), runtime_platform.machine()
                )
        else:
            if not (shutil.which("cargo") and shutil.which("rustc")):
                raise RuntimeError("Building pyrn from source requires Cargo and rustc.")
            host = subprocess.check_output(["rustc", "--print", "host-tuple"], text=True).strip()
            tag_platform = os.environ.get("PYRN_WHEEL_PLATFORM") or wheel_platform(
                host, sysconfig.get_platform(), runtime_platform.machine()
            )
            target_environment: dict[str, str] = {}
            if tag_platform.startswith("macosx_"):
                _, major, minor, *_ = tag_platform.split("_")
                target_environment["MACOSX_DEPLOYMENT_TARGET"] = f"{major}.{minor}"
            target_triple = os.environ.get("PYRN_CARGO_TARGET", host)
            cmd = [
                "cargo",
                "build",
                "--release",
                "--target",
                target_triple,
                "--package",
                "pyrn-cli",
                "--message-format=json-render-diagnostics",
            ]
            if (Path(self.root) / "Cargo.lock").is_file():
                cmd.insert(2, "--locked")
            build = subprocess.run(
                cmd,
                cwd=self.root,
                env={**os.environ, **target_environment},
                text=True,
                stdout=subprocess.PIPE,
                check=True,
            )
            artifacts = [json.loads(line) for line in build.stdout.splitlines()]
            (executable,) = [
                Path(item["executable"])
                for item in artifacts
                if item.get("reason") == "compiler-artifact"
                and item.get("target", {}).get("name") == "pyrn"
                and item.get("executable")
            ]

        platform_parts = tag_platform.split(".")
        windows = all(part.startswith("win") for part in platform_parts)
        binary_name = "pyrn.exe" if windows else "pyrn"
        build_data["force_include"][str(executable)] = f"pyrn/_bin/{binary_name}"
        build_data["tag"] = f"py3-none-{tag_platform}"
        build_data["pure_python"] = False
