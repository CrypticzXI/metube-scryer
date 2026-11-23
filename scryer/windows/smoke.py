"""Exercise the portable Windows package using its embedded Python, without a browser.

All downloaded media and state live in one temporary directory. The local fixture
needs ALLOW_PRIVATE_ADDRESSES only in the launched smoke-test process.
"""

from __future__ import annotations

import argparse
import functools
import http.server
import json
import math
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
import wave


def request(base, path, payload=None):
    body = None if payload is None else json.dumps(payload).encode('utf-8')
    headers = {} if body is None else {'Content-Type': 'application/json'}
    call = urllib.request.Request(base + path, data=body, headers=headers)
    with urllib.request.urlopen(call, timeout=10) as response:
        content = response.read(2 * 1024 * 1024 + 1)
        if len(content) > 2 * 1024 * 1024:
            raise AssertionError('Smoke response exceeded size limit')
        return content


def json_request(base, path, payload=None):
    return json.loads(request(base, path, payload))


def wait_until(process, operation, *, timeout=90):
    deadline = time.monotonic() + timeout
    last_error = None
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise AssertionError(f'Portable launcher exited with status {process.returncode}')
        try:
            result = operation()
            if result:
                return result
        except (OSError, ValueError, urllib.error.URLError) as error:
            last_error = error
        time.sleep(0.4)
    raise AssertionError(f'Portable package did not become ready: {last_error}')


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, _format, *_args):
        pass


def write_fixture(path):
    rate = 16000
    samples = b''.join(struct.pack('<h', int(2000 * math.sin(2 * math.pi * 440 * i / rate))) for i in range(rate))
    with wave.open(str(path), 'wb') as audio:
        audio.setnchannels(1)
        audio.setsampwidth(2)
        audio.setframerate(rate)
        audio.writeframes(samples)


def download_selection(catalog):
    # Read the service's live choices; no parallel copy of the option IDs.
    for kind in catalog['download_type']['options']:
        formats = kind.get('format', {}).get('options', [])
        for format_ in formats:
            if format_.get('text', '').upper() == 'WAV':
                result = {
                    'download_type': kind['id'],
                    'format': format_['id'],
                    'quality': format_['quality']['default'],
                }
                if 'audio_tags' in kind:
                    result['audio_tags'] = kind['audio_tags']['default']
                return result
    raise AssertionError('The live format catalog has no WAV audio choice')


def completed_download(base, downloads):
    history = json_request(base, '/history')
    for item in history['done']:
        if item.get('status') == 'error':
            raise AssertionError(f'Packaged download failed: {item.get("error", "unknown error")}')
        filename = item.get('filename')
        if item.get('status') == 'finished' and filename:
            path = (downloads / filename).resolve()
            if not path.is_relative_to(downloads.resolve()):
                raise AssertionError('Packaged download escaped its temporary download directory')
            if path.is_file() and path.stat().st_size > 44:
                return path
    return None


def stop(process):
    if process.poll() is None:
        # The launcher and multiprocessing workers form one owned process tree.
        subprocess.run(['taskkill', '/PID', str(process.pid), '/T', '/F'], check=False,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                       creationflags=getattr(subprocess, 'CREATE_NO_WINDOW', 0))
    try:
        process.wait(timeout=15)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--bundle', required=True, type=Path)
    args = parser.parse_args()
    bundle = args.bundle.resolve(strict=True)
    runtime = bundle / 'runtime' / 'python.exe'
    launcher = bundle / 'scryer' / 'windows' / 'launcher.py'
    assert runtime.is_file(), 'Embedded Python was not packaged'
    assert launcher.is_file(), 'Windows launcher was not packaged'
    for binary, option in [('runtime/ffmpeg/bin/ffmpeg.exe','-version'),
                           ('runtime/ffmpeg/bin/ffprobe.exe','-version'),
                           ('runtime/deno.exe','--version')]:
        subprocess.run([str(bundle / binary),option], check=True, capture_output=True, timeout=20)
    with tempfile.TemporaryDirectory(prefix='metube-windows-smoke-') as temporary:
        root = Path(temporary)
        downloads = root / 'downloads'
        state = root / 'state'
        transient = root / 'temp'
        fixture = root / 'fixture'
        for directory in (downloads, state, transient, fixture):
            directory.mkdir()
        write_fixture(fixture / 'portable-smoke.wav')
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), functools.partial(QuietHandler, directory=str(fixture)))
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        environment = os.environ.copy()
        environment.update({
            'HOST': '127.0.0.1', 'PORT': '18881', 'URL_PREFIX': '',
            'YOUTUBE_LIBRARY_BIND': '127.0.0.1:18090',
            'SCRYER_INDEXER_URL': 'http://127.0.0.1:18090',
            'DOWNLOAD_DIR': str(downloads), 'AUDIO_DOWNLOAD_DIR': str(downloads),
            'STATE_DIR': str(state), 'TEMP_DIR': str(transient),
            'ALLOW_PRIVATE_ADDRESSES': 'true', 'PYTHONUTF8': '1',
            'YTDL_NIGHTLY_UPDATE_TIME': '',
        })
        log_path = root / 'launcher.log'
        try:
            with log_path.open('w', encoding='utf-8') as log:
                process = subprocess.Popen([str(runtime), str(launcher), '--no-browser'], cwd=bundle,
                                           env=environment, stdout=log, stderr=subprocess.STDOUT,
                                           creationflags=getattr(subprocess, 'CREATE_NO_WINDOW', 0))
                try:
                    base = 'http://127.0.0.1:18881'
                    status = wait_until(process, lambda: json_request(base, '/api/status'))
                    assert status.get('version') and status.get('yt_dlp'), 'Indexer health is unavailable'
                    assert json_request(base, '/version'), 'MeTube version is unavailable'
                    history = json_request(base, '/history')
                    assert history == {'done': [], 'queue': [], 'pending': []}, 'Smoke state was not isolated'
                    assert b'<html' in request(base, '/').lower(), 'Browser UI was not packaged'
                    search = json_request(base, '/api/scryer/search', {'request': {'query': ''}, 'sources': []})
                    assert search.get('ok', {}).get('results') == [], 'Indexer SDK response is invalid'
                    selection = download_selection(json_request(base, '/formats'))
                    selection.update({'url': f'http://127.0.0.1:{server.server_port}/portable-smoke.wav', 'auto_start': True})
                    added = json_request(base, '/add', selection)
                    assert added.get('status') == 'ok', f'Fixture download was not queued: {added}'
                    output = wait_until(process, lambda: completed_download(base, downloads), timeout=90)
                    print(f'Portable browser smoke passed: UI, metadata worker, SDK search, and WAV download ({output.stat().st_size} bytes)')
                finally:
                    stop(process)
        except BaseException:
            if log_path.exists():
                print(log_path.read_text(encoding='utf-8', errors='replace')[-20000:], file=sys.stderr)
            raise
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)


if __name__ == '__main__':
    main()
