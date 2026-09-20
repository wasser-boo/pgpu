#!/usr/bin/env bash
# Baut das Router-Image (inkl. gpu-agent aus dem praxis-gpu-agent-Repo).
# Usage: IMAGE_TAG=vayayo/praxis-gpu-router:0.2 bash deploy/build.sh
#
# Multi-Arch (Vast-Boxen=amd64, NAS=arm64) bauen+pushen — Builder einmalig:
#   docker buildx create --name multiarch --driver docker-container --use
# dann aus dem Repo-Root (TAG/Version anheben):
#   docker buildx build --platform linux/amd64,linux/arm64 -f deploy/Dockerfile \
#     --build-context agent=../praxis-gpu-agent \
#     -t vayayo/praxis-gpu-router:0.15 -t vayayo/praxis-gpu-router:latest \
#     --provenance=false --push .
set -euo pipefail
cd "$(dirname "$0")/.."

: "${IMAGE_TAG:=praxis-gpu-router:dev}"
: "${AGENT_REPO_DIR:=../praxis-gpu-agent}"

if [ ! -d "$AGENT_REPO_DIR/src" ]; then
  echo "praxis-gpu-agent-Checkout fehlt: $AGENT_REPO_DIR" >&2
  echo "git clone ssh://git@forgejo.the.grid/Marvin/praxis-gpu-agent.git $AGENT_REPO_DIR" >&2
  exit 1
fi

docker build -f deploy/Dockerfile \
  --build-context "agent=$AGENT_REPO_DIR" \
  -t "$IMAGE_TAG" .
echo "Image $IMAGE_TAG gebaut (enthält /gpu-agent für COPY --from in Forks)."