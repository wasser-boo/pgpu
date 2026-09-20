#!/usr/bin/env bash
# Baut das STT-Sidecar-Image (Vosk de/ja).
# Usage: IMAGE_TAG=vayayo/praxis-stt:0.1 bash deploy/stt/build.sh
#
# Multi-Arch (vosk hat aarch64-Wheels) bauen+pushen — aus deploy/stt/:
#   docker buildx build --platform linux/amd64,linux/arm64 \
#     -t vayayo/praxis-stt:0.2 -t vayayo/praxis-stt:latest \
#     --provenance=false --push .
set -euo pipefail
cd "$(dirname "$0")"
: "${IMAGE_TAG:=praxis-stt:dev}"
docker build -t "$IMAGE_TAG" .
echo "Image $IMAGE_TAG gebaut."