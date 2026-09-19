#!/usr/bin/env bash
# Baut das STT-Sidecar-Image (Vosk de/ja).
# Usage: IMAGE_TAG=vayayo/praxis-stt:0.1 bash deploy/stt/build.sh
set -euo pipefail
cd "$(dirname "$0")"
: "${IMAGE_TAG:=praxis-stt:dev}"
docker build -t "$IMAGE_TAG" .
echo "Image $IMAGE_TAG gebaut."