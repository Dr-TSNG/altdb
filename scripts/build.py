#!/usr/bin/env python3
"""Build Android ARM64 with cargo-ndk; optionally flash the KernelSU module via adb."""

import argparse
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tomllib
import uuid

ROOT = Path(__file__).resolve().parents[1]


def run(*args, cwd=ROOT):
    command = list(map(str, args))
    print("+ " + " ".join(command), flush=True)
    with subprocess.Popen(
        command,
        cwd=cwd,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="replace",
        bufsize=1,
    ) as process:
        for line in process.stdout:
            print(line, end="", flush=True)
        code = process.wait()
    if code:
        raise subprocess.CalledProcessError(code, command)


def flash(adb, archive, reboot):
    remote = f"/data/local/tmp/altdb-{uuid.uuid4().hex}.zip"

    def su(command):
        return adb + ["shell", "su -c " + shlex.quote(command)]

    try:
        run(*adb, "push", archive, remote)
        run(*su(f"/data/adb/ksud module install {remote}"))
    finally:
        # Delete only this invocation's temporary ZIP; preserve installer errors.
        subprocess.run(su(f"rm -f {remote}"), check=False)
    if reboot:
        print("Module installed; rebooting device.", flush=True)
        run(*su("/system/bin/reboot"))
    else:
        print(
            "Module installed. Follow the installer's instructions if a reboot is required.",
            flush=True,
        )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", nargs="?", choices=["build", "flash"], default="build")
    parser.add_argument(
        "--binary-only",
        action="store_true",
        help="build and verify altdb ELF only; skip WebUI and module ZIP",
    )
    parser.add_argument("--reboot", action="store_true", help="reboot after a successful flash")
    parser.add_argument("-s", "--serial", help="adb device serial or IP:port for flash")
    args = parser.parse_args()
    if args.action == "flash" and args.binary_only:
        parser.error("flash requires a module ZIP; --binary-only is not supported")
    if args.action != "flash" and (args.reboot or args.serial is not None):
        parser.error("--reboot and --serial require the flash action")
    run(sys.executable, ROOT / "scripts/fetch-deps.py")
    run(
        "cargo",
        "ndk",
        "-t",
        "arm64-v8a",
        "-P",
        "30",
        "build",
        "--locked",
        "--release",
        "--bin",
        "altdb",
    )
    run(sys.executable, ROOT / "scripts/package.py", "--check-binary")
    if not args.binary_only:
        run("pnpm", "install", "--frozen-lockfile", cwd=ROOT / "webui")
        run("pnpm", "run", "build", cwd=ROOT / "webui")
        run(sys.executable, ROOT / "scripts/package.py")
    if args.action == "flash":
        adb = ["adb"] + (["-s", args.serial] if args.serial else [])
        version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["package"][
            "version"
        ]
        flash(adb, ROOT / f"dist/altdb-v{version}-arm64.zip", args.reboot)


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        raise SystemExit(error.returncode) from None
