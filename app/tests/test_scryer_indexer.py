"""The opt-in indexer bridge uses real local HTTP peers, never yt-dlp."""

from __future__ import annotations

import json

import pytest
from aiohttp import web

import main
import scryer_indexer


async def _proxy(aiohttp_client, aiohttp_server, *, worker=None, worker_url='', prefix='/'):
    if worker is not None:
        server = await aiohttp_server(worker)
        worker_url = str(server.make_url('/')).rstrip('/')
    app = web.Application(middlewares=[main.cross_origin_guard])
    scryer_indexer.install(app, url_prefix=prefix, worker_url=worker_url)
    return await aiohttp_client(app)


def _envelope():
    return {
        'request': {'query': 'Sidemen Sundays', 'season': 2026, 'episode': 47, 'limit': 20},
        'metadata_connection': {
            'url': 'http://scryer:8080', 'username': 'admin', 'password': 'private-fixture-password',
        },
        'sources': [{'series_name': 'Sidemen Sundays', 'channel_url': 'https://www.youtube.com/@Sidemen'}],
    }


async def test_disabled_routes_return_404_without_session(aiohttp_client, aiohttp_server):
    client = await _proxy(aiohttp_client, aiohttp_server)
    assert (await client.get('/api/status')).status == 404
    assert (await client.post('/api/scryer/search', json=_envelope())).status == 404
    assert scryer_indexer._SESSION not in client.server.app
    assert main.Config._DEFAULTS['SCRYER_INDEXER_URL'] == ''
    assert 'SCRYER_INDEXER_URL' not in main.config.frontend_safe()


async def test_success_forwards_exact_body_but_no_proxy_credentials_and_closes_session(
    aiohttp_client, aiohttp_server, caplog,
):
    received = []
    worker = web.Application()

    async def status(_request):
        return web.json_response({'version': '0.1.0', 'yt_dlp': '2026.11.1', 'internal': 'not exposed'})

    async def search(request):
        received.append(await request.read())
        assert 'Authorization' not in request.headers
        assert 'Cookie' not in request.headers
        assert 'Origin' not in request.headers
        return web.json_response({'ok': {'results': [], 'next_cursor': None}})

    worker.router.add_get('/api/status', status)
    worker.router.add_post('/api/scryer/search', search)
    client = await _proxy(aiohttp_client, aiohttp_server, worker=worker, prefix='/metube/')
    status_response = await client.get('/metube/api/status')
    assert await status_response.json() == {'version': '0.1.0', 'yt_dlp': '2026.11.1'}
    assert (await client.get('/api/status')).status == 404
    raw = json.dumps(_envelope(), separators=(',', ':')).encode()
    response = await client.post(
        '/metube/api/scryer/search', data=raw,
        headers={'Content-Type': 'application/json', 'Authorization': 'Bearer reverse-proxy-secret', 'Cookie': 'session=proxy-secret'},
    )
    assert response.status == 200
    assert await response.json() == {'ok': {'results': [], 'next_cursor': None}}
    assert received == [raw]
    assert 'private-fixture-password' not in caplog.text
    assert 'reverse-proxy-secret' not in caplog.text
    session = client.server.app[scryer_indexer._SESSION]
    assert not session.closed
    await client.close()
    assert session.closed


@pytest.mark.parametrize('payload', [
    {'version': '0.1.0', 'yt_dlp': None},
    {'version': '0.1.0', 'yt_dlp': ''},
    {'error': 'http://secret:password@worker'},
])
async def test_status_requires_healthy_worker_ytdlp(aiohttp_client, aiohttp_server, payload):
    worker = web.Application()

    async def status(_request):
        return web.json_response(payload)

    worker.router.add_get('/api/status', status)
    client = await _proxy(aiohttp_client, aiohttp_server, worker=worker)
    response = await client.get('/api/status')
    assert response.status == 503
    assert 'password' not in await response.text()


@pytest.mark.parametrize('body', ['{"password":"private-fixture-password",', '[]', 'null'])
async def test_malformed_search_body_is_not_forwarded(aiohttp_client, aiohttp_server, body):
    worker = web.Application()

    async def search(_request):
        pytest.fail('malformed input reached worker')

    worker.router.add_post('/api/scryer/search', search)
    client = await _proxy(aiohttp_client, aiohttp_server, worker=worker)
    response = await client.post('/api/scryer/search', data=body)
    assert response.status == 400
    assert 'private-fixture-password' not in await response.text()


@pytest.mark.parametrize('chunked', [False, True])
async def test_search_body_is_bounded_before_worker(aiohttp_client, aiohttp_server, chunked):
    worker = web.Application()

    async def search(_request):
        pytest.fail('oversized input reached worker')

    worker.router.add_post('/api/scryer/search', search)
    client = await _proxy(aiohttp_client, aiohttp_server, worker=worker)
    raw = b'x' * (scryer_indexer.MAX_REQUEST_BYTES + 1)

    async def chunks():
        yield raw[:65536]
        yield raw[65536:]

    response = await client.post('/api/scryer/search', data=chunks() if chunked else raw)
    assert response.status == 413


@pytest.mark.parametrize('kind', ['http_error', 'invalid_json', 'invalid_shape', 'large', 'large_chunked'])
async def test_worker_failures_are_bounded_and_redacted(aiohttp_client, aiohttp_server, kind, caplog):
    worker = web.Application()

    async def search(request):
        await request.read()
        if kind == 'http_error':
            return web.Response(status=500, text='http://user:private-fixture-password@scryer')
        if kind == 'invalid_json':
            return web.Response(text='private-fixture-password')
        if kind == 'invalid_shape':
            return web.json_response({'ok': {'results': 'private-fixture-password'}})
        if kind == 'large':
            return web.Response(body=b'x' * (scryer_indexer.MAX_RESPONSE_BYTES + 1))
        response = web.StreamResponse()
        await response.prepare(request)
        try:
            for _ in range(129):
                await response.write(b'x' * 16384)
            await response.write_eof()
        except ConnectionResetError:
            pass  # The bounded reader may stop before the worker finishes.
        return response

    worker.router.add_post('/api/scryer/search', search)
    client = await _proxy(aiohttp_client, aiohttp_server, worker=worker)
    response = await client.post('/api/scryer/search', json=_envelope())
    assert response.status == 503
    assert 'private-fixture-password' not in await response.text()
    assert 'private-fixture-password' not in caplog.text


async def test_sdk_error_details_never_echo_worker_message(aiohttp_client, aiohttp_server):
    worker = web.Application()

    async def search(_request):
        return web.json_response({'err': {
            'code': 'invalid_config',
            'public_message': 'http://user:private-fixture-password@scryer',
            'debug_message': 'private-fixture-password',
            'details': {'password': 'private-fixture-password'},
        }})

    worker.router.add_post('/api/scryer/search', search)
    client = await _proxy(aiohttp_client, aiohttp_server, worker=worker)
    response = await client.post('/api/scryer/search', json=_envelope())
    assert response.status == 200
    payload = await response.json()
    assert payload['err']['code'] == 'invalid_config'
    assert payload['err']['debug_message'] is None
    assert payload['err']['details'] is None
    assert 'private-fixture-password' not in json.dumps(payload)


async def test_redirects_do_not_forward_scryer_credentials(aiohttp_client, aiohttp_server):
    target_calls = []
    target = web.Application()

    async def redirected(request):
        target_calls.append(await request.read())
        return web.json_response({'ok': {'results': []}})

    target.router.add_route('*', '/capture', redirected)
    target_server = await aiohttp_server(target)
    worker = web.Application()

    async def redirect(_request):
        raise web.HTTPTemporaryRedirect(str(target_server.make_url('/capture')))

    worker.router.add_get('/api/status', redirect)
    worker.router.add_post('/api/scryer/search', redirect)
    client = await _proxy(aiohttp_client, aiohttp_server, worker=worker)
    assert (await client.get('/api/status')).status == 503
    assert (await client.post('/api/scryer/search', json=_envelope())).status == 503
    assert target_calls == []


async def test_unavailable_worker_returns_503(aiohttp_client, aiohttp_server):
    worker = await aiohttp_server(web.Application())
    url = str(worker.make_url('/'))
    await worker.close()
    client = await _proxy(aiohttp_client, aiohttp_server, worker_url=url)
    assert (await client.get('/api/status')).status == 503
    assert (await client.post('/api/scryer/search', json=_envelope())).status == 503


async def test_existing_cross_origin_guard_still_applies(aiohttp_client, aiohttp_server, monkeypatch):
    monkeypatch.setattr(main, '_cors_origins', [])
    worker = web.Application()

    async def search(_request):
        pytest.fail('cross-origin request reached worker')

    worker.router.add_post('/api/scryer/search', search)
    client = await _proxy(aiohttp_client, aiohttp_server, worker=worker)
    response = await client.post(
        '/api/scryer/search', json=_envelope(),
        headers={'Origin': 'https://untrusted.example', 'Sec-Fetch-Site': 'cross-site'},
    )
    assert response.status == 403


@pytest.mark.parametrize('value', [
    'http://user:private-fixture-password@127.0.0.1:8090',
    'http://127.0.0.1:8090?password=private-fixture-password',
    'file:///private-fixture-password',
    'http://localhost:invalid',
])
def test_worker_config_validation_never_echoes_input(value):
    with pytest.raises(ValueError) as error:
        scryer_indexer._worker_base(value)
    assert 'private-fixture-password' not in str(error.value)
