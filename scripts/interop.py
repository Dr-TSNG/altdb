#!/usr/bin/env python3
"""Exercise the real Rust daemon against stock adb, on Linux loopback only.

This does not emulate or claim to validate Android SELinux, KernelSU, pm, or reboot.
The test-only daemon command is absent from the Android binary.
"""

import argparse
import concurrent.futures
import hashlib
import json
import os
import pathlib
import select
import shlex
import signal
import socket
import struct
import statistics
import subprocess
import tempfile
import threading
import time

parser = argparse.ArgumentParser()
parser.add_argument("--adb", required=True)
parser.add_argument("--binary", required=True)
parser.add_argument(
    "--latency-only",
    action="store_true",
    help="Measure persistent shell and command latency, then exit",
)
parser.add_argument(
    "--exec-in-repeats",
    type=int,
    default=1,
    help="Repeat large exec-in to exercise flow-control races",
)
args = parser.parse_args()
if args.exec_in_repeats < 1:
    parser.error("--exec-in-repeats must be at least 1")
binary = str(pathlib.Path(args.binary).resolve())
adb = str(pathlib.Path(args.adb).resolve())


def port():
    with socket.socket() as server:
        server.bind(("127.0.0.1", 0))
        return server.getsockname()[1]


def until(fn, timeout=15):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            last = fn()
            if last:
                return last
        except (OSError, AssertionError, KeyError):
            pass
        time.sleep(0.05)
    raise AssertionError(f"condition timed out; last result: {last}")


def exact(sock, length):
    result = bytearray()
    while len(result) < length:
        data = sock.recv(length - len(result))
        if not data:
            raise EOFError("socket closed")
        result.extend(data)
    return bytes(result)


class Echo:
    def __init__(self, address=None):
        self.socket = socket.socket(socket.AF_UNIX if address else socket.AF_INET)
        self.socket.bind(address if address else ("127.0.0.1", 0))
        self.socket.listen()
        self.port = None if address else self.socket.getsockname()[1]
        threading.Thread(target=self.serve, daemon=True).start()

    def serve(self):
        while True:
            try:
                connection, _ = self.socket.accept()
            except OSError:
                return

            def relay(client):
                with client:
                    while True:
                        data = client.recv(65536)
                        if not data:
                            return
                        client.sendall(data)

            threading.Thread(target=relay, args=(connection,), daemon=True).start()

    def close(self):
        self.socket.close()


with tempfile.TemporaryDirectory(prefix="altdb-interop-") as folder:
    root = pathlib.Path(folder)
    state = root / "state"
    environment = dict(os.environ)
    environment["ANDROID_USER_HOME"] = str(root / "android-user")
    pathlib.Path(environment["ANDROID_USER_HOME"]).mkdir()
    server_port = port()
    host = [adb, "-P", str(server_port)]
    runtime_environment = {
        "ANDROID_ROOT": "/system",
        "ANDROID_DATA": "/data",
        "ANDROID_ASSETS": "/system/app",
        "ANDROID_STORAGE": "/storage",
        "ANDROID_RUNTIME_ROOT": "/apex/com.android.runtime",
        "ANDROID_ART_ROOT": "/apex/com.android.art",
        "ANDROID_I18N_ROOT": "/apex/com.android.i18n",
        "ANDROID_TZDATA_ROOT": "/apex/com.android.tzdata",
        "EXTERNAL_STORAGE": "/sdcard",
        "ASEC_MOUNTPOINT": "/mnt/asec",
        "BOOTCLASSPATH": "/apex/test/core.jar:/system/framework/device-specific.jar",
        "DEX2OATBOOTCLASSPATH": "/apex/test/core.jar",
        "SYSTEMSERVERCLASSPATH": "/system/framework/services.jar",
        "STANDALONE_SYSTEMSERVER_JARS": "/apex/test/service.jar",
    }
    excluded_environment = {
        "ASH_STANDALONE": "1",
        "KSU": "true",
        "CLASSPATH": "/data/local/tmp/unrelated.jar",
        "LD_LIBRARY_PATH": str(root),
        "ALTDB_TEST_SECRET": "must-not-reach-client-shells",
    }
    daemon_environment = dict(os.environ, **runtime_environment, **excluded_environment)
    daemon = None
    echoes = []

    def run(*command, check=True, input=None):
        result = subprocess.run(
            host + list(command), input=input, capture_output=True, env=environment, timeout=25
        )
        if check and result.returncode != 0:
            raise AssertionError((command, result.returncode, result.stdout, result.stderr))
        return result

    def ctl(op, **fields):
        request = json.dumps({"op": op, **fields}, separators=(",", ":")).encode()
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(15)
            client.connect(str(state / "control.sock"))
            client.sendall(struct.pack("<I", len(request)) + request)
            reply = json.loads(exact(client, struct.unpack("<I", exact(client, 4))[0]))
        assert reply["ok"], reply
        return reply["data"]

    def running():
        status = ctl("status")
        return status if status["state"] == "running" else None

    def start():
        return subprocess.Popen(
            [binary, "test-daemon", str(state)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            env=daemon_environment,
        )

    def connect(address):
        result = run("connect", address)
        assert b"connected" in result.stdout, result.stdout
        until(lambda: run("-s", address, "get-state", check=False).stdout.strip() == b"device")

    def exchange(address, data):
        family = socket.AF_UNIX if isinstance(address, str) else socket.AF_INET
        with socket.socket(family) as client:
            client.settimeout(10)
            client.connect(address)
            client.sendall(data)
            assert exact(client, len(data)) == data

    try:
        daemon = start()
        status = until(running)
        address = status["endpoints"][0]
        rejected = run("connect", address, check=False)
        assert b"connected to" not in rejected.stdout, rejected.stdout
        pairing = ctl("pair_start")["pairing"]
        result = run("pair", pairing["endpoints"][0], pairing["code"])
        assert b"Successfully paired" in result.stdout, result.stdout
        until(lambda: ctl("status")["pairing"] is None)
        connect(address)
        print("PASS stock adb pairing / TLS / unknown-host rejection", flush=True)

        for _ in range(3):
            until(lambda: ctl("status")["connections"] == 1)
            run("disconnect", address)
            until(lambda: ctl("status")["connections"] == 0)
            connect(address)
        until(lambda: ctl("status")["connections"] == 1)
        print("PASS repeated disconnect/reconnect clears connection counts", flush=True)

        run("disconnect", address)
        until(lambda: ctl("status")["connections"] == 0)
        connected = sum(row["message"] == "Paired host connected" for row in ctl("logs"))
        # Let authentication time out in the worker while its supervisor cannot
        # reply, then process the queued registration and its cleanup together.
        daemon.send_signal(signal.SIGSTOP)
        try:
            paused_at = time.monotonic()
            rejected = run("connect", address, check=False)
            assert b"connected to" not in rejected.stdout, rejected.stdout
            run("disconnect", address, check=False)
            time.sleep(max(0, 12 - (time.monotonic() - paused_at)))
        finally:
            daemon.send_signal(signal.SIGCONT)
        until(
            lambda: sum(row["message"] == "Paired host connected" for row in ctl("logs"))
            > connected
        )
        until(lambda: ctl("status")["connections"] == 0)
        connect(address)
        print("PASS authentication reply timeout leaves no stale connection", flush=True)

        # Exercise both exec boundaries, including raw and PTY shell variants.
        for command in [("shell", "-T", "env"), ("shell", "-tt", "env"), ("exec-out", "env")]:
            values = dict(
                line.split("=", 1)
                for line in run("-s", address, *command).stdout.decode().splitlines()
                if "=" in line
            )
            for key, value in runtime_environment.items():
                assert values.get(key) == value, (command, key, values.get(key))
            assert not excluded_environment.keys() & values.keys(), values
            assert values["TMPDIR"] == values["HOME"] == "/tmp", values
            assert values["LOGNAME"] == values["USER"], values
            assert values["PATH"] == "/usr/bin:/bin", values
        print(
            "PASS Android runtime environment survives service/shell without unrelated vars",
            flush=True,
        )

        # One persistent shell isolates input/response latency from command
        # startup. Keep binary payload checks; timings are reported, not used
        # as a machine-dependent CI pass threshold.
        session = subprocess.Popen(
            host + ["-s", address, "shell", "-T", "cat"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env=environment,
            bufsize=0,
        )
        samples = []
        try:
            for index in range(41):
                payload = bytes([index])
                started = time.perf_counter()
                session.stdin.write(payload)
                assert select.select([session.stdout], [], [], 5)[0], "shell response timed out"
                assert os.read(session.stdout.fileno(), 1) == payload
                if index:
                    samples.append((time.perf_counter() - started) * 1000)
        finally:
            session.stdin.close()
            try:
                session.wait(timeout=5)
            except subprocess.TimeoutExpired:
                session.kill()
                session.wait()
            session.stdout.close()
        command_samples = []
        for _ in range(8):
            started = time.perf_counter()
            assert run("-s", address, "shell", "printf ok").stdout == b"ok"
            command_samples.append((time.perf_counter() - started) * 1000)
        print(
            "LATENCY "
            + json.dumps(
                {
                    "shell_rtt_ms_p50": round(statistics.median(samples), 2),
                    "shell_rtt_ms_p95": round(sorted(samples)[37], 2),
                    "shell_rtt_samples": len(samples),
                    "command_ms_p50": round(statistics.median(command_samples), 2),
                }
            ),
            flush=True,
        )
        if args.latency_only:
            raise SystemExit(0)

        def concurrent_shell(index):
            data = run("-s", address, "shell", f"sleep 0.1; printf stream-{index}").stdout
            assert data == f"stream-{index}".encode(), data

        with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
            list(pool.map(concurrent_shell, range(32)))
        print("PASS concurrent multiplexed shell streams", flush=True)

        result = run(
            "-s", address, "shell", "printf stdout; printf stderr >&2; exit 19", check=False
        )
        assert (result.returncode, result.stdout, result.stderr) == (19, b"stdout", b"stderr"), (
            result
        )
        result = run("-s", address, "exec-out", "printf '\\000\\377'")
        assert result.stdout == b"\x00\xff", result.stdout
        result = run("-s", address, "shell", "-tt", "printf pty-ok")
        assert b"pty-ok" in result.stdout
        exec_file = root / "exec-in.bin"
        for _ in range(args.exec_in_repeats):
            exec_bytes = os.urandom(1024 * 1024 + 7)
            run("-s", address, "exec-in", f"cat > {shlex.quote(str(exec_file))}", input=exec_bytes)
            until(lambda: exec_file.exists() and exec_file.read_bytes() == exec_bytes)
        print(
            f"PASS large exec-in: {args.exec_in_repeats} transfers, exact binary comparison",
            flush=True,
        )
        # A remote stream closing must also terminate a session which traps HUP.
        pid_file = root / "session.pid"
        session = subprocess.Popen(
            host + ["-s", address, "shell", f"echo $$ > {pid_file}; trap '' HUP; sleep 300"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env=environment,
        )
        until(lambda: pid_file.exists() and pid_file.read_text().strip())
        session_pid = int(pid_file.read_text())
        session.terminate()
        session.wait(timeout=5)
        until(lambda: not pathlib.Path(f"/proc/{session_pid}").exists())
        print(
            "PASS shell v2 stdout / stderr / exit code / PTY / binary exec-in-out / disconnect cleanup",
            flush=True,
        )

        source = root / "source with space.bin"
        source.write_bytes(os.urandom(2 * 1024 * 1024 + 123))
        remote = root / "device files" / "binary.bin"
        run("-s", address, "push", str(source), str(remote))
        pulled = root / "pulled.bin"
        run("-s", address, "pull", str(remote), str(pulled))
        assert (
            hashlib.sha256(source.read_bytes()).digest()
            == hashlib.sha256(pulled.read_bytes()).digest()
        )
        tree = root / "tree"
        tree.mkdir()
        (tree / "empty").write_bytes(b"")
        (tree / "unicode-测试").write_text("hello")
        run("-s", address, "push", str(tree), str(root / "remote-tree"))
        run("-s", address, "pull", str(root / "remote-tree"), str(root / "tree-copy"))
        assert (root / "tree-copy" / "unicode-测试").read_text() == "hello"
        print("PASS SYNC v2 binary / large file / directories / Unicode / empty files", flush=True)

        echo = Echo()
        echoes.append(echo)
        forward = int(run("-s", address, "forward", "tcp:0", f"tcp:{echo.port}").stdout.strip())
        exchange(("127.0.0.1", forward), b"forward\x00" * 8192)
        abstract = f"altdb-test-{os.getpid()}"
        local = Echo("\0" + abstract)
        echoes.append(local)
        forward_unix = int(
            run("-s", address, "forward", "tcp:0", f"localabstract:{abstract}").stdout.strip()
        )
        exchange(("127.0.0.1", forward_unix), b"abstract-forward")
        reverse = int(run("-s", address, "reverse", "tcp:0", f"tcp:{echo.port}").stdout.strip())
        exchange(("127.0.0.1", reverse), b"reverse\x00" * 8192)
        reverse_path = str(root / "reverse.sock")
        run("-s", address, "reverse", f"localfilesystem:{reverse_path}", f"tcp:{echo.port}")
        exchange(reverse_path, b"filesystem-reverse")
        assert b"tcp:" in run("-s", address, "reverse", "--list").stdout
        run("-s", address, "reverse", "--remove-all")
        until(lambda: not pathlib.Path(reverse_path).exists())
        run("-s", address, "forward", "--remove-all")
        print("PASS TCP / abstract forward and TCP / filesystem reverse / cleanup", flush=True)

        result = run("-s", address, "root", check=False)
        assert b"disabled" in result.stdout + result.stderr
        config = ctl("status")["config"]
        config["allow_adb_root"] = True
        ctl("configure", config=config)
        until(running)
        connect(address)
        run("-s", address, "root")
        time.sleep(1)
        status = until(running)
        assert status["root"] and status["endpoints"][0] == address
        for command in [("shell", "-T", "env"), ("shell", "-tt", "env"), ("exec-out", "env")]:
            values = dict(
                line.split("=", 1)
                for line in run("-s", address, *command).stdout.decode().splitlines()
                if "=" in line
            )
            assert values["PATH"] == "/usr/bin:/bin:/data/adb/ksu/bin", (command, values)
        run("-s", address, "unroot")
        time.sleep(1)
        assert not until(running)["root"]
        assert run("-s", address, "shell", "printenv PATH").stdout.strip() == b"/usr/bin:/bin"
        assert not run("-s", address, "shell", "printenv ENV", check=False).stdout
        print(
            "PASS global root policy / root PATH / stable connection and port / unroot (policy only)",
            flush=True,
        )

        fp = ctl("status")["hosts"][0]["fingerprint"]
        ctl("revoke", fingerprint=fp)
        until(lambda: ctl("status")["connections"] == 0)
        until(lambda: run("-s", address, "get-state", check=False).stdout.strip() != b"device")
        run("disconnect", address, check=False)
        assert b"connected to" not in run("connect", address, check=False).stdout
        pairing = ctl("pair_start")["pairing"]
        for _ in range(10):
            wrong = "000000" if pairing["code"] != "000000" else "000001"
            run("pair", pairing["endpoints"][0], wrong, check=False)
        until(lambda: ctl("status")["pairing"] is None)
        print("PASS revocation and ten-failure pairing cutoff", flush=True)

        pairing = ctl("pair_start")["pairing"]
        run("pair", pairing["endpoints"][0], pairing["code"])
        until(lambda: len(ctl("status")["hosts"]) == 1)
        identity = json.loads((state / "identity.json").read_text())["guid"]
        daemon.terminate()
        daemon.wait(timeout=10)
        daemon = start()
        status = until(running)
        assert not status["root"] and status["hosts"][0]["fingerprint"] == fp
        assert json.loads((state / "identity.json").read_text())["guid"] == identity
        config = status["config"]
        with socket.socket() as occupied:
            occupied.bind(("127.0.0.1", 0))
            occupied.listen()
            config.update(port_mode="fixed", fixed_port=occupied.getsockname()[1])
            ctl("configure", config=config)
            until(lambda: ctl("status")["state"] == "error")
            assert ctl("status")["endpoints"] == []
        config.update(port_mode="random", enabled=False)
        ctl("configure", config=config)
        until(lambda: ctl("status")["state"] == "disabled")
        enabled = ctl("toggle")
        assert enabled["config"] == dict(config, enabled=True)
        until(running)
        disabled = ctl("toggle")
        assert disabled["config"] == config
        until(lambda: ctl("status")["state"] == "disabled")
        assert json.loads((state / "config.json").read_text()) == config
        ctl("toggle")
        until(running)
        assert all(pairing["code"] not in row["message"] for row in ctl("logs"))
        print(
            "PASS persistent identity / safe restart / fixed collision / disable-enable / log hygiene",
            flush=True,
        )
        print("ALL LOOPBACK INTEROPERABILITY CHECKS PASSED", flush=True)
    finally:
        if daemon:
            daemon.terminate()
            try:
                daemon.wait(timeout=10)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait()
            if daemon.stderr:
                diagnostics = daemon.stderr.read().decode(errors="replace")
                if diagnostics:
                    print("Fixture diagnostics:\n" + diagnostics, flush=True)
        for echo in echoes:
            echo.close()
        run("kill-server", check=False)
