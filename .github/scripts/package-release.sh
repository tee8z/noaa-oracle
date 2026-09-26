#!/usr/bin/env bash
# Package the release's binaries (see .github/workflows/release.yml).
#
#   package-release.sh <version> <x86_64|aarch64> <binaries dir> <duckdb dir>
#
# The binaries dir holds oracle and daemon; the oracle embeds its UI. The
# DuckDB dir holds libduckdb.so (from fetch_duckdb.py). Writes to release/:
#   noaa-oracle-<version>-<arch>-linux.zip        binaries, libduckdb.so, configs, scripts
#   noaa-oracle-<version>-x86_64-linux-cargo.tar.gz   bin/oracle and bin/daemon (x86_64 only)
set -euo pipefail

[[ $# -eq 4 ]] || { sed -n '2,10p' "$0" >&2; exit 2; }
VERSION=$1
ARCH=$2
BIN_DIR=$3
DUCKDB_DIR=$4
case "$ARCH" in
  x86_64) TITLE="NOAA Oracle v${VERSION}" ;;
  aarch64) TITLE="NOAA Oracle v${VERSION} (ARM64/aarch64)" ;;
  *) echo "unsupported architecture: $ARCH" >&2; exit 2 ;;
esac
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "bad version: $VERSION" >&2; exit 2; }

PKG_NAME="noaa-oracle-${VERSION}-${ARCH}-linux"
rm -rf "$PKG_NAME"
mkdir -p "${PKG_NAME}/bin" "${PKG_NAME}/lib" "${PKG_NAME}/config" release

install -m 755 "$BIN_DIR/oracle" "$BIN_DIR/daemon" "${PKG_NAME}/bin/"
# The oracle finds it through its $ORIGIN/../lib runpath.
install -m 755 "$DUCKDB_DIR/libduckdb.so" "${PKG_NAME}/lib/"

# Copy and rename config examples to usable defaults
cp config/oracle.example.toml "${PKG_NAME}/config/oracle.toml"
cp config/daemon.example.toml "${PKG_NAME}/config/daemon.toml"
# Signed uploads must use the same origin as the packaged oracle config.
sed -i 's|^base_url = "http://localhost:9800"$|base_url = "http://127.0.0.1:9800"|' "${PKG_NAME}/config/daemon.toml"

# Create setup script for first-time deployment
cat > "${PKG_NAME}/setup.sh" << 'EOF'
#!/bin/bash
set -e
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

echo "NOAA Oracle Setup"
echo "================="

# Create data directories
echo "Creating data directories..."
mkdir -p weather_data event_data logs data

echo "Each service creates its signing key with mode 0600 on first startup."
echo "Existing keys remain in place. Back up both keys after startup."
echo "The oracle needs its original key to attest existing events."

echo ""
echo "Setup complete! You can now run:"
echo "  ./run-oracle.sh  - Start the oracle server"
echo "  ./run-daemon.sh  - Start the data fetching daemon"
EOF

# Create run script for oracle
cat > "${PKG_NAME}/run-oracle.sh" << 'EOF'
#!/bin/bash
set -e
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Create data directories if missing
mkdir -p weather_data event_data logs

export RUST_LOG="${RUST_LOG:-info}"
export NOAA_ORACLE_CONFIG="${NOAA_ORACLE_CONFIG:-$SCRIPT_DIR/config/oracle.toml}"
exec ./bin/oracle "$@"
EOF

# Create run script for daemon
cat > "${PKG_NAME}/run-daemon.sh" << 'EOF'
#!/bin/bash
set -e
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

mkdir -p data logs

export RUST_LOG="${RUST_LOG:-info}"
export NOAA_DAEMON_CONFIG="${NOAA_DAEMON_CONFIG:-$SCRIPT_DIR/config/daemon.toml}"
exec ./bin/daemon "$@"
EOF
chmod +x "${PKG_NAME}/setup.sh" "${PKG_NAME}/run-oracle.sh" "${PKG_NAME}/run-daemon.sh"

UNDERLINE=$(printf '%*s' "${#TITLE}" '' | tr ' ' '=')
cat > "${PKG_NAME}/README.txt" << EOF
${TITLE}
${UNDERLINE}

Quick Start:
  1. Run ./setup.sh to create the data directories
  2. Run ./run-oracle.sh to start the oracle and create its key (port 9800)
  3. Run ./run-daemon.sh to create its key and start fetching NOAA weather data
  4. Copy the npub the daemon logs at startup into uploader_pubkeys
     in config/oracle.toml and restart the oracle; until then the
     oracle rejects the daemon's uploads (401)

Configuration:
  - Edit config/oracle.toml for oracle settings
  - Edit config/daemon.toml for daemon settings
  - Keep daemon base_url equal to oracle remote_url, including scheme, host, and port
  - The run scripts load these files; --config or NOAA_ORACLE_CONFIG /
    NOAA_DAEMON_CONFIG can select another file

Directories:
  - config/       Configuration files
  - bin/          Service binaries
  - lib/          The DuckDB library the oracle loads
  - weather_data/ Downloaded parquet files (created on first run)
  - event_data/   DLC event database (created on first run)

Security:
  - oracle_private_key.pem is your signing key - BACK IT UP!
  - daemon_private_key.pem signs the daemon's uploads; its npub is
    what uploader_pubkeys lists, so back it up too
  - Each service creates its key with mode 0600 if the key is missing
  - Keep both keys secret and preserve them across upgrades

For more info: https://github.com/tee8z/noaa-oracle
EOF

rm -f "release/${PKG_NAME}.zip"
zip -r "release/${PKG_NAME}.zip" "${PKG_NAME}"
rm -rf "$PKG_NAME"

if [[ "$ARCH" == x86_64 ]]; then
  # The binaries alone, for deployments that supply DuckDB themselves.
  # The oracle embeds its UI, so the binaries are all it needs.
  CARGO_PKG=$(mktemp -d)
  chmod 755 "$CARGO_PKG"
  install -D -m 755 "$BIN_DIR/oracle" "$CARGO_PKG/bin/oracle"
  install -D -m 755 "$BIN_DIR/daemon" "$CARGO_PKG/bin/daemon"
  tar czf "release/noaa-oracle-${VERSION}-x86_64-linux-cargo.tar.gz" -C "$CARGO_PKG" .
  rm -rf "$CARGO_PKG"
fi

ls -lh release/
