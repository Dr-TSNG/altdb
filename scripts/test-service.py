#!/usr/bin/env python3
"""Linux regression for the real watchdog script and a loopback-only daemon.

Android boot properties are simulated. This does not restart a phone or verify
Android process survival; framework recovery policy has separate Rust tests.
"""

import argparse
import json
import os
from pathlib import Path
import select
import shlex
import shutil
import signal
import socket
import subprocess
import tempfile
import time


def until(check, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            result = check()
            if result:
                return result
        except (OSError, ValueError, subprocess.SubprocessError):
            pass
        time.sleep(0.05)
    raise AssertionError("condition timed out")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--adb", required=True, type=Path)
    parser.add_argument("--busybox", type=Path, help="run module scripts in standalone BusyBox")
    args = parser.parse_args()
    binary = str(args.binary.resolve())
    with tempfile.TemporaryDirectory(prefix="altdb-service-") as folder:
        root = Path(folder)
        state = root / "state"
        boot = root / "boot"
        starts = root / "starts"
        module = root / "module"
        bin_dir = module / "bin"
        bin_dir.mkdir(parents=True)
        shell = [str(args.busybox.resolve()), "sh"] if args.busybox else ["/bin/sh"]
        busybox = args.busybox.resolve() if args.busybox else bin_dir / "busybox"
        if not args.busybox:
            busybox.write_text('#!/bin/sh\nexec "$@"\n')
        else:
            # Android's external sh does not provide standalone BusyBox applets.
            # Catch launchers that silently escape into an external shell.
            (bin_dir / "sh").write_text('#!/bin/sh\necho "external sh used" >&2\nexit 127\n')
        boot.write_text("0\n")
        scripts = Path(__file__).resolve().parents[1] / "module"
        for name in ("service.sh", "emulated-soft-reboot.sh"):
            source = (scripts / name).read_text()
            assert source.count("STATE=/data/adb/altdb") == 1
            (module / name).write_text(
                source.replace("STATE=/data/adb/altdb", f"STATE={shlex.quote(str(state))}")
            )
        (module / "hotinstall.sh").write_text(
            (scripts / "hotinstall.sh")
            .read_text()
            .replace("BUSYBOX=/data/adb/ksu/bin/busybox", f"BUSYBOX={shlex.quote(str(busybox))}")
        )
        (bin_dir / "getprop").write_text(f"#!/bin/sh\ncat {shlex.quote(str(boot))}\n")
        (bin_dir / "altdb").write_text(
            "#!/bin/sh\n"
            'if [ "$1" = ctl ]; then\ncase "$2" in\n'
            f"stop) exec {shlex.quote(binary)} test-ctl {shlex.quote(str(state))} "
            '\'{"op":"stop"}\' ;;\n'
            f"status) exec {shlex.quote(binary)} test-ctl {shlex.quote(str(state))} "
            '\'{"op":"status"}\' ;;\nesac\nexit 1\nfi\n'
            # A daemon must not inherit the watchdog's lock descriptor.
            "[ ! -e /proc/self/fd/9 ] || exit 99\n"
            f"echo $$ >> {shlex.quote(str(starts))}\n"
            f"exec {shlex.quote(binary)} test-daemon {shlex.quote(str(state))}\n"
        )
        for executable in bin_dir.iterdir():
            executable.chmod(0o755)
        environment = dict(os.environ)
        if args.busybox:
            environment["ASH_STANDALONE"] = "1"
        environment["PATH"] = f"{bin_dir}:{environment['PATH']}"
        environment["ANDROID_USER_HOME"] = str(root / "android-user")
        Path(environment["ANDROID_USER_HOME"]).mkdir()
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            adb_port = probe.getsockname()[1]
        host = [str(args.adb.resolve()), "-P", str(adb_port)]
        watchdogs = []
        hot_installs = []
        session = None

        def launch():
            process = subprocess.Popen(
                shell + [str(module / "service.sh")], env=environment, start_new_session=True
            )
            watchdogs.append(process)
            return process

        def soft_reboot():
            subprocess.run(
                shell + [str(module / "emulated-soft-reboot.sh")],
                env=environment,
                check=True,
                timeout=25,
            )

        def hot_install(success=True):
            process = subprocess.Popen(
                shell + [str(module / "hotinstall.sh")],
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            hot_installs.append(process)
            output, _ = process.communicate(timeout=30)
            assert process.returncode == (0 if success else 1), output.decode(errors="replace")
            return output

        def locked():
            return (
                subprocess.run(
                    ["flock", "-n", str(state / "service.lock"), "true"], timeout=2
                ).returncode
                == 1
            )

        def ctl(op):
            result = subprocess.run(
                [binary, "test-ctl", str(state), json.dumps({"op": op})],
                capture_output=True,
                text=True,
                timeout=5,
            )
            reply = json.loads(result.stdout)
            assert reply["ok"], reply
            return reply["data"]

        def running():
            status = ctl("status")
            return status if status["state"] == "running" else None

        def adb(*command):
            return subprocess.run(
                host + list(command),
                env=environment,
                check=True,
                capture_output=True,
                timeout=20,
            ).stdout

        def exchange(data):
            assert session.poll() is None
            session.stdin.write(data)
            assert select.select([session.stdout], [], [], 5)[0], "shell response timed out"
            assert os.read(session.stdout.fileno(), len(data)) == data

        try:
            watchdog = launch()
            until(lambda: (state / "service.lock").exists())
            until(locked)
            duplicates = [launch() for _ in range(4)]
            assert all(process.wait(timeout=3) == 0 for process in duplicates)
            assert not starts.exists() and watchdog.poll() is None
            print("PASS watchdog single instance before boot completed", flush=True)

            boot.write_text("1\n")
            status = until(running)
            address = status["endpoints"][0]
            pid = int(starts.read_text().strip())
            pairing = ctl("pair_start")["pairing"]
            assert b"Successfully paired" in adb("pair", pairing["endpoints"][0], pairing["code"])
            assert b"connected" in adb("connect", address)
            session = subprocess.Popen(
                host + ["-s", address, "shell", "-T", "cat"],
                env=environment,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                bufsize=0,
            )
            exchange(b"a")
            boot.write_text("0\n")
            soft_reboot()
            watchdog.send_signal(signal.SIGHUP)
            duplicates = [launch() for _ in range(4)]
            assert all(process.wait(timeout=3) == 0 for process in duplicates)
            time.sleep(2)
            exchange(b"b")
            boot.write_text("1\n")
            exchange(b"c")
            assert starts.read_text().splitlines() == [str(pid)]
            assert ctl("status")["endpoints"] == [address]
            print(
                "PASS boot property reset / duplicate scripts / HUP preserve TLS shell and port",
                flush=True,
            )

            session.stdin.close()
            session.wait(timeout=5)
            session.stdout.close()
            session = None
            os.kill(pid, signal.SIGKILL)
            until(lambda: len(starts.read_text().splitlines()) == 2)
            until(running)
            assert watchdog.poll() is None
            print("PASS crash restart and watchdog lock not inherited by daemon", flush=True)

            # Installing an update alone must not disconnect the running daemon.
            (module / "update").touch()
            old_pid = int(starts.read_text().splitlines()[-1])
            time.sleep(2)
            assert starts.read_text().splitlines()[-1] == str(old_pid)
            assert running()
            boot.write_text("0\n")
            soft_reboot()
            assert watchdog.wait(timeout=3) == 0
            assert not Path(f"/proc/{old_pid}").exists()
            assert not (state / "control.sock").exists()
            assert not locked()

            # KernelSU replaces the module directory after the pre-reboot hook.
            retired = root / "retired"
            module.rename(retired)
            shutil.copytree(retired, module)
            (module / "update").unlink()
            watchdog = launch()
            until(locked)
            assert len(starts.read_text().splitlines()) == 2
            boot.write_text("1\n")
            status = until(running)
            assert len(starts.read_text().splitlines()) == 3
            address = status["endpoints"][0]
            assert b"connected" in adb("connect", address)
            assert adb("-s", address, "shell", "echo", "updated").strip() == b"updated"
            print("PASS update stops old daemon/watchdog and replacement keeps pairing", flush=True)

            (module / "disable").touch()
            assert watchdog.wait(timeout=10) == 0
            assert not (state / "control.sock").exists()
            (module / "disable").unlink()
            watchdog = launch()
            until(running)
            (module / "remove").touch()
            assert watchdog.wait(timeout=10) == 0
            assert not (state / "control.sock").exists()
            print("PASS disable / re-enable / remove and automatic lock release", flush=True)

            (module / "remove").unlink()
            count = len(starts.read_text().splitlines())
            hot_install()
            status = until(running)
            assert len(starts.read_text().splitlines()) == count + 1
            fingerprint = status["hosts"][0]["fingerprint"]
            old_pid = int(starts.read_text().splitlines()[-1])
            address = status["endpoints"][0]
            assert b"connected" in adb("connect", address)
            session = subprocess.Popen(
                host + ["-s", address, "shell", "-T", "cat"],
                env=environment,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                bufsize=0,
            )
            exchange(b"h")
            print("PASS hot install starts a stopped service", flush=True)

            # Hot install swaps directories before running the hook and leaves
            # no update flag. The new hook must still stop the old processes.
            boot.write_text("0\n")
            module.rename(root / "hot-retired")
            shutil.copytree(root / "hot-retired", module)
            assert not (module / "update").exists()
            hot_install()
            session.wait(timeout=5)
            session.stdin.close()
            session.stdout.close()
            session = None
            assert not Path(f"/proc/{old_pid}").exists()
            assert not (state / "control.sock").exists()
            until(locked)
            assert len(starts.read_text().splitlines()) == count + 1
            boot.write_text("1\n")
            status = until(running)
            assert len(starts.read_text().splitlines()) == count + 2
            assert status["hosts"][0]["fingerprint"] == fingerprint
            duplicate = launch()
            assert duplicate.wait(timeout=3) == 0
            address = status["endpoints"][0]
            assert b"connected" in adb("connect", address)
            assert adb("-s", address, "shell", "printf hot-updated") == b"hot-updated"
            (module / "disable").touch()
            until(lambda: not (state / "control.sock").exists())
            until(lambda: not locked())
            (module / "disable").unlink()
            print(
                "PASS hot replacement stops old shell/watchdog, preserves pairing and starts once",
                flush=True,
            )

            (module / "update").touch()
            boot.write_text("0\n")
            count = len(starts.read_text().splitlines())
            watchdog = launch()
            until(locked)
            soft_reboot()
            assert watchdog.wait(timeout=3) == 0
            assert len(starts.read_text().splitlines()) == count
            print("PASS update stops watchdog before boot completed", flush=True)

            # There may be no control socket while the watchdog is backing off.
            (bin_dir / "altdb").write_text(
                '#!/bin/sh\n[ "$1" != ctl ] || exit 1\n'
                f"echo $$ >> {shlex.quote(str(starts))}\n"
                "echo 'simulated daemon startup failure' >&2\nexit 1\n"
            )
            boot.write_text("1\n")
            watchdog = launch()
            until(lambda: len(starts.read_text().splitlines()) == count + 3)
            soft_reboot()
            assert watchdog.wait(timeout=3) == 0
            assert len(starts.read_text().splitlines()) == count + 3
            assert not locked()
            print("PASS update stops watchdog during crash backoff", flush=True)

            output = hot_install(success=False)
            assert b"control socket did not become available" in output
            assert "simulated daemon startup failure" in (state / "service.log").read_text()
            print("PASS failed hot startup reports failure and retains daemon error", flush=True)
        finally:
            (module / "disable").touch()
            if session:
                session.kill()
                session.wait()
            for process in watchdogs:
                if process.poll() is None:
                    process.terminate()
                    process.wait(timeout=10)
            for process in hot_installs:
                try:
                    os.killpg(process.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                process.wait(timeout=10)
            subprocess.run(host + ["kill-server"], env=environment, capture_output=True, timeout=10)


if __name__ == "__main__":
    main()
