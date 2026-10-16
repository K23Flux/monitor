#!/usr/bin/env bash
# Builds sb-agent for linux/amd64 and linux/arm64 into sbmon/bin/, where the
# hub embeds them (src/sbmon/mod.rs). Run before `cargo build --release`; CI
# does it in release.yml.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p bin
(cd agent && go test ./... && go vet ./...)
for arch in amd64 arm64; do
  (cd agent && CGO_ENABLED=0 GOOS=linux GOARCH=$arch go build -buildvcs=false -trimpath -ldflags="-s -w" -o "../bin/sb-agent-linux-$arch" .)
done
(cd bin && sha256sum sb-agent-linux-amd64 sb-agent-linux-arm64 > checksums.txt)
echo "sb-agent $(sed -n 's/^\tversion *= "\(.*\)"/\1/p' agent/main.go) built into sbmon/bin"
