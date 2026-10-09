#!/usr/bin/env python3
"""Check that release package, lockfile, Nix, and chart versions agree."""
import sys
from pathlib import Path
import re
import tomllib

version = sys.argv[1] if len(sys.argv) > 1 else tomllib.loads(Path("Cargo.toml").read_text())["workspace"]["package"]["version"]
if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
    raise SystemExit("Release version must have the form MAJOR.MINOR.PATCH")
workspace = tomllib.loads(Path("Cargo.toml").read_text())
versions = {"workspace": workspace["workspace"]["package"]["version"]}
packages = ("daemon", "noaa-oracle-core", "oracle")
locked = tomllib.loads(Path("Cargo.lock").read_text())["package"]
for name in packages:
    matches = [package for package in locked if package["name"] == name]
    if len(matches) != 1:
        raise SystemExit(f"Expected one lockfile entry for {name}")
    versions[f"Cargo.lock:{name}"] = matches[0]["version"]
for member in workspace["workspace"]["members"]:
    manifest = tomllib.loads(Path(member, "Cargo.toml").read_text())
    if manifest["package"]["version"] != {"workspace": True}:
        raise SystemExit(f"{member} must inherit the workspace version")
flake = Path("flake.nix").read_text()
for name in ("oracle", "daemon"):
    match = re.search(r'pname = "' + name + r'";\s+version = "([^"]+)";', flake)
    if match is None:
        raise SystemExit(f"Missing Nix package version for {name}")
    versions[f"flake.nix:{name}"] = match.group(1)
for name in ("noaa-oracle", "noaa-daemon"):
    chart = Path("deploy/helm", name, "Chart.yaml")
    match = re.search(r'^appVersion: "([^"]+)"$', chart.read_text(), re.MULTILINE)
    if match is None:
        raise SystemExit(f"Missing appVersion in {chart}")
    versions[str(chart)] = match.group(1)
for source, actual in versions.items():
    if actual != version:
        raise SystemExit(f"{source}: expected {version}, found {actual}")
print(f"Validated all package and chart versions: {version}")
