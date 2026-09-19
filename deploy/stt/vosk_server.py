"""Private CPU Vosk: legacy WAV/HTTP plus native PCM/WebSocket utterances.

No transcripts, audio, credentials or raw third-party exception messages are
logged. WebSocket clients send config, PCM16 mono frames, then {"eof": 1}.
"""
import asyncio
from concurrent.futures import ThreadPoolExecutor
import io
import json
import logging
import os
from pathlib import Path
import wave

LANGUAGES = tuple(x for x in os.environ.get("STT_LANGUAGES", "de,ja").split(",") if x)
MODEL_ROOT = Path(os.environ.get("VOSK_MODEL_ROOT", "/data/vosk"))
MAX_BYTES = 4 * 1024 * 1024
MAX_SECONDS = 120
MAX_WS_BYTES = 16 * 1024 * 1024
MAX_WS_SECONDS = 300
WS_TIMEOUT = 120
MODELS = {}


def pcm_from_wav(data):
    try:
        with wave.open(io.BytesIO(data), "rb") as audio:
            if (audio.getnchannels(), audio.getsampwidth(), audio.getframerate(), audio.getcomptype()) != (1, 2, 16000, "NONE"):
                raise ValueError("Expected mono 16000 Hz signed PCM16 WAV")
            count = audio.getnframes()
            if not 0 < count <= 16000 * MAX_SECONDS:
                raise ValueError("Audio must contain a nonempty utterance of at most 120 seconds")
            pcm = audio.readframes(count)
            if len(pcm) != count * 2:
                raise ValueError("Truncated WAV")
            return pcm
    except (wave.Error, EOFError) as exc:
        raise ValueError("Invalid WAV") from exc


def recognizer_for(language, rate):
    if language not in LANGUAGES:
        raise ValueError("Supported languages: fr, de, ja")
    from vosk import Model, KaldiRecognizer
    if language not in MODELS:
        directory = MODEL_ROOT / language
        if not directory.is_dir():
            raise FileNotFoundError("Requested language model is not provisioned")
        MODELS[language] = Model(str(directory))
    return KaldiRecognizer(MODELS[language], rate)


class Utterance:
    def __init__(self, language, rate):
        self.recognizer = recognizer_for(language, rate)
        self.parts = []

    def feed(self, pcm):
        # Accumulate finalized segments internally; the transport acknowledges
        # every PCM frame with an empty partial and sends the full text at EOF.
        for start in range(0, len(pcm), 8000):
            if self.recognizer.AcceptWaveform(pcm[start:start + 8000]):
                self.parts.append(json.loads(self.recognizer.Result()).get("text", ""))

    def finish(self):
        self.parts.append(json.loads(self.recognizer.FinalResult()).get("text", ""))
        return " ".join(p for p in self.parts if p)


def transcribe(data, language):
    if language not in LANGUAGES:
        raise ValueError("Supported languages: fr, de, ja; language selection is required")
    pcm = pcm_from_wav(data)
    utterance = Utterance(language, 16000)
    utterance.feed(pcm)
    return {"text": utterance.finish(), "language": language}


def create_app():
    from aiohttp import web, WSMsgType
    default_language = os.environ.get("VOSK_DEFAULT_LANGUAGE", "de")
    if default_language not in LANGUAGES:
        raise ValueError("VOSK_DEFAULT_LANGUAGE must be fr, de or ja")
    # Cancellation cannot forcibly stop a C++ recognizer. A single worker
    # keeps CPU inference serialized even if a client disconnects mid-call.
    executor = ThreadPoolExecutor(max_workers=1, thread_name_prefix="vosk")
    lock = asyncio.Lock()
    app = web.Application(client_max_size=MAX_BYTES)

    async def work(function, *args):
        return await asyncio.get_running_loop().run_in_executor(executor, function, *args)

    async def health(request):
        ready = all(language in MODELS for language in LANGUAGES)
        return web.json_response({"ready": ready, "loaded_languages": sorted(MODELS)}, status=200 if ready else 503)

    async def post(request):
        if request.content_type != "audio/wav":
            return web.json_response({"error": "Content-Type must be audio/wav"}, status=415)
        if request.headers.get("Transfer-Encoding"):
            return web.json_response({"error": "Send Content-Length, not a chunked upload"}, status=400)
        if not request.content_length or not 0 < request.content_length <= MAX_BYTES:
            return web.json_response({"error": "WAV must be nonempty and at most 4 MiB"}, status=413)
        language = request.query.get("language", "")
        if language not in LANGUAGES:
            return web.json_response({"error": "Select language=fr, de, or ja"}, status=400)
        async def recognize():
            data = await asyncio.wait_for(request.read(), 30)
            async with lock:
                return await work(transcribe, data, language)
        try:
            return web.json_response(await asyncio.wait_for(recognize(), 120))
        except (ValueError, TimeoutError, asyncio.TimeoutError):
            return web.json_response({"error": "Invalid audio or request deadline exceeded"}, status=400)
        except FileNotFoundError:
            return web.json_response({"error": "Language model not provisioned"}, status=503)
        except Exception as error:
            logging.error("Vosk HTTP request failed (%s)", type(error).__name__)
            return web.json_response({"error": "Transcription failed"}, status=500)

    async def websocket(request):
        # Native Praxis sends no browser Origin. Do not silently disable
        # cross-origin protection for arbitrary websites on a private API.
        if request.headers.get("Origin"):
            return web.json_response({"error": "Browser origins are not enabled"}, status=403)
        # Native Praxis deliberately rejects query strings in server URLs.
        # /de, /fr and /ja therefore select models without relaxing that guard.
        language = request.match_info.get("language") or request.query.get("language", default_language)
        if language not in LANGUAGES:
            return web.json_response({"error": "Select language=fr, de, or ja"}, status=400)
        ws = web.WebSocketResponse(max_msg_size=MAX_WS_BYTES, heartbeat=30)
        if not ws.can_prepare(request).ok:
            return web.json_response({"error": "WebSocket upgrade required"}, status=400)
        await ws.prepare(request)

        async def converse():
            async with lock:
                utterance = None
                rate = 0
                received = 0
                async for message in ws:
                    if message.type == WSMsgType.TEXT:
                        if len(message.data) > 4096:
                            raise ValueError("Control message too large")
                        control = json.loads(message.data)
                        if not isinstance(control, dict):
                            raise ValueError("Expected JSON object")
                        if set(control) == {"config"} and utterance is None:
                            config = control["config"]
                            rate = config.get("sample_rate") if isinstance(config, dict) else None
                            if isinstance(rate, bool) or not isinstance(rate, (int, float)) or not 8000 <= rate <= 48000 or int(rate) != rate:
                                raise ValueError("Unsupported sample rate")
                            utterance = await work(Utterance, language, int(rate))
                        elif control == {"eof": 1} and utterance is not None and received:
                            text = await work(utterance.finish)
                            await ws.send_json({"text": text, "language": language})
                            return
                        else:
                            raise ValueError("Expected one config, PCM, then eof")
                    elif message.type == WSMsgType.BINARY:
                        pcm = message.data
                        received += len(pcm)
                        if utterance is None or not pcm or len(pcm) % 2 or received > MAX_WS_BYTES or received > rate * MAX_WS_SECONDS * 2:
                            raise ValueError("Invalid or oversized PCM utterance")
                        await work(utterance.feed, pcm)
                        # Standard vosk-server/Praxis expects one response per
                        # chunk before sending the next chunk or EOF.
                        await ws.send_json({"partial": ""})
                    elif message.type == WSMsgType.ERROR:
                        return
        try:
            await asyncio.wait_for(converse(), WS_TIMEOUT)
        except FileNotFoundError:
            await ws.send_json({"error": "Language model not provisioned"})
        except (ValueError, TimeoutError, asyncio.TimeoutError):
            await ws.send_json({"error": "Invalid speech request or deadline exceeded"})
        except (ConnectionError, RuntimeError):
            # Runtime errors from a recognizer must also be reported without
            # exposing its exception text. Ignore writes to disconnected peers.
            if not ws.closed:
                try:
                    await ws.send_json({"error": "Transcription failed"})
                except (ConnectionError, RuntimeError):
                    pass
        except Exception as error:
            logging.error("Vosk WebSocket request failed (%s)", type(error).__name__)
            if not ws.closed:
                await ws.send_json({"error": "Transcription failed"})
        finally:
            await ws.close()
        return ws

    async def cleanup(application):
        executor.shutdown(wait=False, cancel_futures=True)
    app.on_cleanup.append(cleanup)
    app.router.add_get('/health', health)
    app.router.add_post('/transcribe', post)
    app.router.add_get('/', websocket)
    app.router.add_get('/ws', websocket)
    app.router.add_get('/ws/{language}', websocket)
    app.router.add_get('/{language}', websocket)
    return app


def main():
    from aiohttp import web
    from vosk import Model
    for language in LANGUAGES:
        directory = MODEL_ROOT / language
        if directory.is_dir():
            MODELS[language] = Model(str(directory))
        else:
            logging.warning("Missing Vosk model for %s; provision before use", language)
    # NetBird netstack forwards the private VPN endpoint to loopback.
    # Im Container: 0.0.0.0 (Compose-Netzwerk), Host-Mapping regelt die Grenze.
    host = os.environ.get("VOSK_BIND", "127.0.0.1")
    port = int(os.environ.get("VOSK_PORT", "2700"))
    web.run_app(create_app(), host=host, port=port, access_log=None, print=None)


if __name__ == '__main__':
    main()
