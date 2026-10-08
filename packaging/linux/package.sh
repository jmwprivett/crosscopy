#!/usr/bin/env bash
# Builds dist/crosscopy-<version>-linux-x86_64.tar.gz. The binaries sit at
# the archive root, which is the layout the in-app updater expects.
#
# Usage: packaging/linux/package.sh <version>
set -euo pipefail

VERSION="${1:?usage: package.sh <version>}"
cd "$(dirname "$0")/../.."

cargo build --release --locked -p crosscopy-tray -p crosscopy

STAGE=dist/linux
rm -rf "$STAGE"
mkdir -p "$STAGE"
cp target/release/crosscopy-tray target/release/crosscopy "$STAGE/"
cp packaging/linux/install.sh packaging/linux/crosscopy.desktop "$STAGE/"
cp assets/app-icon.png "$STAGE/crosscopy.png"
chmod 755 "$STAGE/crosscopy-tray" "$STAGE/crosscopy" "$STAGE/install.sh"

tar -C "$STAGE" -czf "dist/crosscopy-$VERSION-linux-x86_64.tar.gz" .
rm -rf "$STAGE"
echo "Built dist/crosscopy-$VERSION-linux-x86_64.tar.gz"
