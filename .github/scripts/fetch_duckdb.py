#!/usr/bin/env python3
"""Download the DuckDB library the oracle links against, verified.

The version and hashes come from flake.nix (duckdbVersion, duckdbSha256), so
the release workflow and the Nix build always use the same library.

    fetch_duckdb.py <x86_64-linux|aarch64-linux> <destination>

Writes <destination>/libduckdb.so and the C headers, and prints the directory.
"""

import hashlib
import io
import re
import sys
import urllib.request
import zipfile
from pathlib import Path

NIX32 = "0123456789abcdfghijklmnpqrsvwxyz"
ARCHES = {"x86_64-linux": "amd64", "aarch64-linux": "arm64"}
MEMBERS = ("libduckdb.so", "duckdb.h", "duckdb_extension.h")


def nix32_decode(text: str, size: int = 32) -> bytes:
    """Decode Nix's base-32 hash notation (as nix-prefetch-url prints)."""
    if len(text) != (size * 8 + 4) // 5:
        raise ValueError(f"expected a {size}-byte Nix base-32 hash")
    out = bytearray(size)
    for n, char in enumerate(reversed(text)):
        digit = NIX32.index(char)
        bit = n * 5
        index, shift = divmod(bit, 8)
        out[index] |= (digit << shift) & 0xFF
        carry = digit >> (8 - shift)
        if index + 1 < size:
            out[index + 1] |= carry
        elif carry:
            raise ValueError("Nix base-32 hash has too many bits")
    return bytes(out)


def pinned(system: str) -> tuple[str, bytes]:
    flake = (Path(__file__).resolve().parents[2] / "flake.nix").read_text()
    version = re.search(r'duckdbVersion = "([0-9.]+)";', flake)
    digest = re.search(r'"' + re.escape(system) + r'" = "([0-9a-z]{52})";', flake)
    if version is None or digest is None:
        raise SystemExit(f"flake.nix pins no DuckDB version or hash for {system}")
    return version.group(1), nix32_decode(digest.group(1))


def main() -> None:
    if len(sys.argv) != 3 or sys.argv[1] not in ARCHES:
        raise SystemExit(__doc__)
    system, destination = sys.argv[1], Path(sys.argv[2])
    version, expected = pinned(system)
    url = (
        f"https://github.com/duckdb/duckdb/releases/download/v{version}/"
        f"libduckdb-linux-{ARCHES[system]}.zip"
    )
    with urllib.request.urlopen(url, timeout=120) as response:
        archive = response.read()
    actual = hashlib.sha256(archive).digest()
    if actual != expected:
        raise SystemExit(
            f"{url}: sha256 {actual.hex()} does not match flake.nix ({expected.hex()})"
        )
    destination.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(io.BytesIO(archive)) as bundle:
        for member in MEMBERS:
            (destination / member).write_bytes(bundle.read(member))
    (destination / "libduckdb.so").chmod(0o755)
    print(f"DuckDB {version} ({system}) verified at {destination}")


if __name__ == "__main__":
    main()
