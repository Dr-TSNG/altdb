#!/usr/bin/env python3
import base64
import pathlib
import struct

folder = pathlib.Path(__file__).resolve().parents[1] / ".cache/fuzz-corpus"
folder.mkdir(parents=True, exist_ok=True)
for command, payload in [
    (b"CNXN", b"host::\0"),
    (b"OPEN", b"shell,v2,raw:id\0"),
    (b"WRTE", b"\0\xff"),
    (b"CLSE", b""),
]:
    number = int.from_bytes(command, "little")
    data = (
        struct.pack("<6I", number, 1, 0, len(payload), sum(payload), number ^ 0xFFFFFFFF) + payload
    )
    (folder / command.decode()).write_bytes(data)
(folder / "shell").write_bytes(b"\0\x02\0\0\0\0\xff")
(folder / "control").write_bytes(
    b'{"op":"configure","config":{"enabled":true,"port_mode":"fixed","fixed_port":5555,"allow_adb_root":false}}'
)
(folder / "dns-loop").write_bytes(b"\xc0\0")
(folder / "host-key").write_bytes(
    base64.b64encode(
        struct.pack("<II", 64, 0) + b"\xff" * 256 + b"\0" * 256 + struct.pack("<I", 65537)
    )
    + b" test@host"
)
