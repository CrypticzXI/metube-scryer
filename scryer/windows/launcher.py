"""Start the portable Windows MeTube and its metadata worker."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import time
from urllib.error import URLError
from urllib.request import urlopen
import webbrowser


def stop_tree(child):
    if child.poll() is None:
        subprocess.run(
            [str(Path(os.environ['SystemRoot']) / 'System32/taskkill.exe'),
             '/PID', str(child.pid), '/T', '/F'],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            creationflags=subprocess.CREATE_NO_WINDOW, timeout=15, check=False,
        )
        if child.poll() is None:
            child.kill()
        child.wait(timeout=10)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--no-browser', action='store_true')
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[2]
    runtime = root / 'runtime'
    env = os.environ.copy()
    defaults = {
        'HOST': '127.0.0.1', 'PORT': '8081',
        'BASE_DIR': str(root), 'DOWNLOAD_DIR': str(root / 'downloads'),
        'STATE_DIR': str(root / 'state'), 'TEMP_DIR': str(root / 'temp'),
        'YOUTUBE_LIBRARY_BIND': '127.0.0.1:8090',
        'SCRYER_INDEXER_URL': 'http://127.0.0.1:8090',
        'YTDLP_BIN': str(runtime / 'yt-dlp.exe'),
        'YTDLP_JS_RUNTIME': 'deno:' + str(runtime / 'deno.exe'),
        'METUBE_VERSION': 'scryer-windows-0.2.0',
        'DELETE_FILE_ON_TRASHCAN': 'false',
        'PYTHONDONTWRITEBYTECODE': '1',
    }
    for key, value in defaults.items():
        env.setdefault(key, value)
    env['PATH'] = os.pathsep.join([str(runtime), str(runtime / 'ffmpeg/bin'), env.get('PATH', '')])
    for key in ['DOWNLOAD_DIR', 'STATE_DIR', 'TEMP_DIR']:
        Path(env[key]).mkdir(parents=True, exist_ok=True)
    children = []
    host = env['HOST'] if env['HOST'] not in ('', '*', '0.0.0.0', '::') else '127.0.0.1'
    url = f'http://{host}:{env["PORT"]}'
    try:
        children.append(subprocess.Popen(
            [str(root / 'bin/scryer-youtube-library-service.exe')], cwd=root, env=env,
            creationflags=subprocess.CREATE_NEW_PROCESS_GROUP | subprocess.CREATE_NO_WINDOW,
        ))
        children.append(subprocess.Popen(
            [str(runtime / 'python.exe'), str(root / 'app/main.py')], cwd=root, env=env,
            creationflags=subprocess.CREATE_NEW_PROCESS_GROUP,
        ))
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            if any(child.poll() is not None for child in children):
                raise RuntimeError('MeTube or the indexer exited during startup; see the log above.')
            try:
                with urlopen(url + '/api/status', timeout=2) as response:
                    if json.load(response).get('yt_dlp'):
                        break
            except (OSError, URLError, ValueError):
                time.sleep(0.5)
        else:
            raise RuntimeError('MeTube did not become ready. Check that its ports are available.')
        print(f'\nMeTube: {url}\nKeep this window open. Press Ctrl+C to stop.\n', flush=True)
        if not args.no_browser:
            webbrowser.open(url)
        while all(child.poll() is None for child in children):
            time.sleep(0.5)
        return 1
    except KeyboardInterrupt:
        return 0
    finally:
        for child in reversed(children):
            stop_tree(child)


if __name__ == '__main__':
    raise SystemExit(main())
