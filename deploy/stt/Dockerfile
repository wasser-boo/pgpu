# STT-Sidecar: privater CPU-Vosk (Protokoll wie fork-comfyui).
# Modelle werden beim Start provisioniert (de/ja per STT_LANGUAGES).
FROM python:3.11-slim-bookworm

RUN apt-get update && apt-get install -y --no-install-recommends wget unzip ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && pip install --no-cache-dir aiohttp==3.9.5 vosk==0.3.45

WORKDIR /app
COPY vosk_server.py .
COPY provision.py .

ENV VOSK_MODEL_ROOT=/data/vosk \
    VOSK_BIND=0.0.0.0 \
    VOSK_PORT=2700 \
    STT_LANGUAGES=de,ja \
    STT_MODELS_URL=https://alphacephei.com/vosk/models

VOLUME /data
CMD ["python3", "provision.py"]