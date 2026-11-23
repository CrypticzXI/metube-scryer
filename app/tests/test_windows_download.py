"""Windows downloads cross a real spawn boundary without parent asyncio state."""

from __future__ import annotations

import asyncio
import io
import multiprocessing
import os
import pickle
import subprocess
import sys
import time
import types
import wave
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock

import pytest
from aiohttp import web

import ytdl
from ytdl import Download, DownloadInfo


def _download(tmp_path, url='http://127.0.0.1/sample.wav'):
    root = tmp_path / 'downloads'
    root.mkdir(exist_ok=True)
    info = DownloadInfo(
        id='sample', title='Sample', url=url, quality='best', download_type='video',
        codec='auto', format='any', folder='', custom_name_prefix='', error=None,
        entry=None, playlist_item_limit=1, split_by_chapters=False, chapter_template='',
    )
    return Download(
        str(root), str(root), '%(title)s.%(ext)s', '%(title)s.%(ext)s',
        'best', 'any', {'noprogress': True, 'retries': 0, 'socket_timeout': 5},
        info, allow_private=True,
    )


async def test_worker_state_excludes_unpicklable_parent_runtime(tmp_path):
    download = _download(tmp_path)
    # A retry may retain all of these. Pickling any one would fail under spawn.
    download.loop = asyncio.get_running_loop()
    download.notifier = lambda _info: None
    download.proc = multiprocessing.get_context('spawn').Process()
    with ThreadPoolExecutor(max_workers=1) as executor:
        download._executor = executor
        state = pickle.loads(pickle.dumps(download._worker_state()))
    assert set(state) == {
        'info', 'download_dir', 'temp_dir', 'state_dir', 'output_template',
        'output_template_chapter', 'allow_private', 'format', 'ytdl_opts',
    }
    assert state['info'].url == download.info.url
    assert state['allow_private'] is True
    assert state['ytdl_opts'] == download.ytdl_opts


def test_worker_reconstructs_child_only_state(tmp_path, monkeypatch):
    download = _download(tmp_path)
    queue = object()
    workers = []

    def child_run(worker):
        workers.append(worker)
        assert worker.loop is None
        assert worker.notifier is None
        assert worker._executor is None
        assert worker.proc is None
        assert worker.status_queue is queue
        assert worker.download_dir == download.download_dir
        assert worker.ytdl_opts == download.ytdl_opts

    monkeypatch.setattr(Download, '_download', child_run)
    ytdl._download_worker_entry(download._worker_state(), queue)
    assert len(workers) == 1


@pytest.mark.parametrize('result', [0, 1, OSError('sensitive-fixture-detail')])
def test_windows_cancel_uses_argv_tree_termination_and_falls_back(tmp_path, monkeypatch, result, caplog):
    download = _download(tmp_path)
    download.proc = MagicMock(pid=12345)
    download.proc.is_alive.return_value = True
    download.loop = MagicMock()
    download.status_queue = MagicMock()
    calls = []

    def taskkill(args, **kwargs):
        calls.append((args, kwargs))
        if isinstance(result, Exception):
            raise result
        return types.SimpleNamespace(returncode=result)

    monkeypatch.setattr(ytdl, '_IS_WINDOWS', True)
    # Windows does not define SIGKILL. Its cancellation path must not read it.
    monkeypatch.setattr(ytdl, 'signal', types.SimpleNamespace(SIGINT=2))
    monkeypatch.setattr(ytdl.subprocess, 'run', taskkill)
    download.cancel()
    assert download.canceled is True
    assert calls[0][0][1:] == ['/PID', '12345', '/T', '/F']
    assert calls[0][0][0].endswith('taskkill.exe')
    assert calls[0][1].get('shell', False) is False
    assert calls[0][1]['timeout'] == 5
    assert calls[0][1]['stdout'] is subprocess.DEVNULL
    assert download.proc.kill.call_count == (0 if result == 0 else 1)
    if result != 0:
        assert 'falling back to worker termination' in caplog.text
    assert 'sensitive-fixture-detail' not in caplog.text
    download.loop.call_later.assert_not_called()
    download.status_queue.put.assert_called_once_with(None)


def test_windows_failed_start_cleanup_does_not_read_sigkill(tmp_path, monkeypatch):
    download = _download(tmp_path)
    download.proc = MagicMock(pid=12345)
    download.proc.is_alive.return_value = True
    monkeypatch.setattr(ytdl, '_IS_WINDOWS', True)
    monkeypatch.setattr(ytdl, 'signal', types.SimpleNamespace())
    kill = MagicMock()
    monkeypatch.setattr(download, '_terminate_windows_tree', kill)
    download._kill_if_alive()
    kill.assert_called_once_with()


@pytest.mark.skipif(sys.platform != 'win32', reason='actual Windows spawn download')
async def test_windows_spawn_downloads_local_media_with_parent_loop_and_executor(tmp_path, aiohttp_server):
    data = io.BytesIO()
    with wave.open(data, 'wb') as wav:
        wav.setnchannels(1)
        wav.setsampwidth(2)
        wav.setframerate(8000)
        wav.writeframes(b'\0\0' * 800)
    media = data.getvalue()

    async def sample(_request):
        return web.Response(body=media, content_type='audio/wav')

    server = web.Application()
    server.router.add_get('/sample.wav', sample)
    worker_server = await aiohttp_server(server)
    download = _download(tmp_path, str(worker_server.make_url('/sample.wav')))
    notifier = types.SimpleNamespace(updated=AsyncMock())
    download.loop = asyncio.get_running_loop()
    download.notifier = notifier
    # The actual start path must avoid pickling all parent runtime references.
    with ThreadPoolExecutor(max_workers=3) as executor:
        download._executor = executor
        try:
            await asyncio.wait_for(download.start(notifier, executor), timeout=45)
            assert download.proc.exitcode == 0
            assert download.info.status == 'finished', download.info.msg
            assert download.info.filename
            assert (Path(download.download_dir) / download.info.filename).read_bytes() == media
            notifier.updated.assert_awaited()
        finally:
            if download.running():
                download.cancel()
                await asyncio.to_thread(download.proc.join, 10)
            download.close()
            await asyncio.to_thread(Download.shutdown_manager)


def _spawn_child_and_wait(queue):
    """A worker with a subprocess, analogous to yt-dlp spawning ffmpeg."""
    child = subprocess.Popen(
        [sys.executable, '-c', 'import time; time.sleep(120)'],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        creationflags=getattr(subprocess, 'CREATE_NO_WINDOW', 0),
    )
    queue.put(child.pid)
    time.sleep(120)


@pytest.mark.skipif(sys.platform != 'win32', reason='Windows process-tree cancellation')
async def test_windows_cancel_stops_worker_and_subprocess(tmp_path):
    import ctypes
    from ctypes import wintypes

    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel.OpenProcess.restype = wintypes.HANDLE
    kernel.WaitForSingleObject.argtypes = [wintypes.HANDLE, wintypes.DWORD]
    kernel.WaitForSingleObject.restype = wintypes.DWORD
    kernel.TerminateProcess.argtypes = [wintypes.HANDLE, wintypes.UINT]
    kernel.TerminateProcess.restype = wintypes.BOOL
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel.CloseHandle.restype = wintypes.BOOL

    context = multiprocessing.get_context('spawn')
    queue = context.Queue()
    proc = context.Process(target=_spawn_child_and_wait, args=(queue,))
    child_handle = None
    try:
        proc.start()
        child_pid = await asyncio.to_thread(queue.get, True, 15)
        # Keep the original process handle, so the assertion/cleanup cannot
        # accidentally refer to a PID reused after the child has exited.
        child_handle = kernel.OpenProcess(0x00100000 | 0x0001, False, child_pid)
        assert child_handle
        download = _download(tmp_path)
        download.proc = proc
        await asyncio.to_thread(download.cancel)
        await asyncio.to_thread(proc.join, 10)
        assert not proc.is_alive()
        assert await asyncio.to_thread(kernel.WaitForSingleObject, child_handle, 10000) == 0
        assert download.canceled is True
    finally:
        if child_handle:
            if kernel.WaitForSingleObject(child_handle, 0) == 258:
                kernel.TerminateProcess(child_handle, 1)
            kernel.CloseHandle(child_handle)
        if proc.is_alive():
            proc.kill()
        await asyncio.to_thread(proc.join, 10)
        proc.close()
        queue.close()
        queue.join_thread()
