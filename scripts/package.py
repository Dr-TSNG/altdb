#!/usr/bin/env python3
"""Build a deterministic KernelSU ZIP; reject an incorrect ELF or page size."""

import hashlib
import argparse
import os
import pathlib
import struct
import subprocess
import tomllib
import zipfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--check-binary", action="store_true", help="only verify the Android ELF")
args = parser.parse_args()
target_dir = pathlib.Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
if not target_dir.is_absolute():
    target_dir = ROOT / target_dir
binary = target_dir / "aarch64-linux-android/release/altdb"
version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["package"]["version"]
module_prop = (
    (ROOT / "module/module.prop").read_text(encoding="utf-8").replace("@CARGO_VERSION@", version)
)
data = binary.read_bytes()
assert data[:6] == b"\x7fELF\x02\x01", "Expected 64-bit little-endian ELF"
assert struct.unpack_from("<H", data, 16)[0] == 3, "Expected PIE (ET_DYN) ELF"
assert struct.unpack_from("<H", data, 18)[0] == 183, "Expected AArch64 ELF"
offset = struct.unpack_from("<Q", data, 32)[0]
size, count = struct.unpack_from("<HH", data, 54)
alignments = []
loads = []
dynamic = None
interpreter = None
for index in range(count):
    entry = offset + index * size
    kind = struct.unpack_from("<I", data, entry)[0]
    file_offset, virtual, _, file_size = struct.unpack_from("<4Q", data, entry + 8)
    if kind == 1:
        alignment = struct.unpack_from("<Q", data, entry + 48)[0]
        assert alignment >= 16384, "PT_LOAD alignment must support 16 KB pages"
        alignments.append(alignment)
        loads.append((virtual, file_offset, file_size))
    elif kind == 2:
        dynamic = (file_offset, file_size)
    elif kind == 3:
        interpreter = data[file_offset : file_offset + file_size].rstrip(b"\0")
assert alignments, "Missing ELF load segments"
assert dynamic and interpreter == b"/system/bin/linker64", "Expected Android PIE executable"
tags = [struct.unpack_from("<QQ", data, n) for n in range(dynamic[0], sum(dynamic), 16)]
strings_address = next(value for tag, value in tags if tag == 5)
strings = next(
    file_offset + strings_address - virtual
    for virtual, file_offset, length in loads
    if virtual <= strings_address < virtual + length
)
needed = []
for tag, value in tags:
    if tag == 1:
        start = strings + value
        needed.append(data[start : data.index(b"\0", start)].decode("ascii"))
assert "libc.so" in needed, "Bionic must be linked dynamically; check NDK search order"
assert set(needed) <= {"libc.so", "libdl.so", "libm.so", "liblog.so"}, (
    f"Unexpected shared dependency: {needed}"
)
print(
    f"Android ARM64 ELF: {binary}\nSHA256 {hashlib.sha256(data).hexdigest()}\n"
    f"PT_LOAD alignment: {alignments}\nShared dependencies: {needed}"
)
if args.check_binary:
    raise SystemExit(0)
assert (
    subprocess.check_output(
        ["git", "rev-parse", "--is-shallow-repository"], cwd=ROOT, text=True
    ).strip()
    == "false"
), "Packaging requires full Git history; run git fetch --unshallow"
version_code = subprocess.check_output(
    ["git", "rev-list", "--count", "HEAD"], cwd=ROOT, text=True
).strip()
module_prop = module_prop.replace("@GIT_COMMIT_COUNT@", version_code)
assert (ROOT / "webui/dist/index.html").is_file(), "Build WebUI first"
files = {
    "bin/altdb": binary,
    "README.md": ROOT / "README.md",
}
for folder, prefix in [(ROOT / "module", ""), (ROOT / "webui/dist", "webroot/")]:
    for path in folder.rglob("*"):
        if path.is_file():
            files[prefix + path.relative_to(folder).as_posix()] = path
output = ROOT / f"dist/altdb-v{version}-arm64.zip"
output.parent.mkdir(exist_ok=True)
with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
    for name, path in sorted(files.items()):
        entry = zipfile.ZipInfo(name, (2026, 1, 1, 0, 0, 0))
        entry.create_system = 3
        entry.external_attr = (
            0o100755 if name.startswith("bin/") or name.endswith(".sh") else 0o100644
        ) << 16
        entry.compress_type = zipfile.ZIP_DEFLATED
        content = module_prop.encode("utf-8") if name == "module.prop" else path.read_bytes()
        if name.endswith(".sh"):
            content = content.replace(b"\r\n", b"\n")
        archive.writestr(entry, content)
digest = hashlib.sha256(output.read_bytes()).hexdigest()
output.with_suffix(".zip.sha256").write_text(f"{digest}  {output.name}\n", encoding="ascii")
print(f"{output}\nSHA256 {digest}\nPT_LOAD alignment: {alignments}\nShared dependencies: {needed}")
