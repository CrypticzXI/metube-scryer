"""Opt-in read-only bridge to the fork's local Scryer indexer worker.

The worker searches and verifies metadata; MeTube's existing queue remains the
only download path. Credentials in search bodies are never logged or persisted.
"""

from __future__ import annotations

import asyncio
import json
from urllib.parse import urlsplit, urlunsplit

from aiohttp import ClientError, ClientSession, ClientTimeout, DummyCookieJar, web


MAX_REQUEST_BYTES = 128 * 1024
MAX_RESPONSE_BYTES = 2 * 1024 * 1024
REQUEST_TIMEOUT_SECONDS = 50
_SESSION = web.AppKey('scryer_indexer_session', ClientSession)


def _worker_base(value: str) -> str | None:
    value = value.strip()
    if not value:
        return None
    try:
        parsed = urlsplit(value)
        if (
            parsed.scheme not in ('http', 'https')
            or not parsed.hostname
            or parsed.username is not None
            or parsed.password is not None
            or parsed.query
            or parsed.fragment
            or any(char.isspace() for char in value)
        ):
            raise ValueError
        # Accessing port also validates its range and spelling.
        _ = parsed.port
        return urlunsplit((parsed.scheme, parsed.netloc, parsed.path.rstrip('/'), '', ''))
    except ValueError:
        # Never put the configured URL (which may accidentally contain secrets)
        # in an exception or a log message.
        raise ValueError('SCRYER_INDEXER_URL must be an HTTP(S) base URL without credentials, query or fragment') from None


def _unavailable() -> web.Response:
    return web.json_response(
        {'error': 'Scryer indexer service is unavailable or returned an invalid response'},
        status=503,
    )


async def _bounded_json_response(response) -> dict:
    if response.status != 200:
        # Redirects and error bodies are deliberately neither followed nor
        # exposed: a worker error can echo a request's Scryer credentials.
        raise ValueError('worker response rejected')
    if response.content_length is not None and response.content_length > MAX_RESPONSE_BYTES:
        raise ValueError('worker response too large')
    body = bytearray()
    async for chunk in response.content.iter_chunked(16 * 1024):
        if len(body) + len(chunk) > MAX_RESPONSE_BYTES:
            raise ValueError('worker response too large')
        body.extend(chunk)
    result = json.loads(body)
    if not isinstance(result, dict):
        raise ValueError('worker response must be an object')
    return result


async def _search_body(request: web.Request) -> bytes:
    if request.content_length is not None and request.content_length > MAX_REQUEST_BYTES:
        raise web.HTTPRequestEntityTooLarge(max_size=MAX_REQUEST_BYTES, actual_size=request.content_length)
    body = bytearray()
    async for chunk in request.content.iter_chunked(16 * 1024):
        if len(body) + len(chunk) > MAX_REQUEST_BYTES:
            raise web.HTTPRequestEntityTooLarge(max_size=MAX_REQUEST_BYTES, actual_size=len(body) + len(chunk))
        body.extend(chunk)
    try:
        data = json.loads(body)
    except (ValueError, UnicodeError):
        raise web.HTTPBadRequest(text='Scryer search body must be a JSON object') from None
    if not isinstance(data, dict):
        raise web.HTTPBadRequest(text='Scryer search body must be a JSON object')
    # Preserve the exact SDK envelope. The worker owns typed validation, and
    # no request field can choose this proxy's upstream URL or HTTP headers.
    return bytes(body)


def _safe_search_response(payload: dict) -> dict:
    if set(payload) == {'ok'}:
        result = payload['ok']
        if not isinstance(result, dict) or not isinstance(result.get('results'), list):
            raise ValueError('invalid search response')
        return payload
    if set(payload) == {'err'} and isinstance(payload['err'], dict):
        messages = {
            'invalid_config': 'Check the YouTube Library indexer service and Scryer metadata settings',
            'auth_failed': 'Scryer metadata authentication failed; check the indexer credentials',
            'rate_limited': 'Indexer search is rate limited; try again later',
            'upstream_unavailable': 'Indexer search is temporarily unavailable; try again later',
            'permanent': 'Indexer search could not be verified; check the source and Scryer metadata settings',
            'unsupported': 'This search is not supported by the indexer service',
        }
        code = payload['err'].get('code')
        if not isinstance(code, str) or code not in messages:
            code = 'upstream_unavailable'
        return {'err': {
            'code': code,
            'public_message': messages[code],
            'debug_message': None,
            'retry_after_seconds': None,
            'details': None,
        }}
    raise ValueError('invalid search response')


def install(app: web.Application, *, url_prefix: str, worker_url: str) -> None:
    """Install routes on MeTube's existing guarded application.

    Empty worker_url disables both routes without creating an HTTP session.
    All requests use the one operator-configured worker base address.
    """
    base = _worker_base(worker_url)
    prefix = url_prefix.rstrip('/')

    async def lifetime(application):
        if base is None:
            yield
            return
        async with ClientSession(
            timeout=ClientTimeout(total=45, connect=5),
            cookie_jar=DummyCookieJar(),
            trust_env=False,
        ) as session:
            application[_SESSION] = session
            yield

    async def status(request):
        if base is None:
            raise web.HTTPNotFound()
        try:
            async with asyncio.timeout(REQUEST_TIMEOUT_SECONDS):
                async with request.app[_SESSION].get(
                    base + '/api/status', allow_redirects=False,
                ) as response:
                    payload = await _bounded_json_response(response)
                yt_dlp = payload.get('yt_dlp')
                version = payload.get('version')
                if not isinstance(yt_dlp, str) or not yt_dlp.strip() or not isinstance(version, str):
                    return _unavailable()
                return web.json_response({'version': version, 'yt_dlp': yt_dlp})
        except (ClientError, TimeoutError, ValueError, UnicodeError):
            return _unavailable()

    async def search(request):
        if base is None:
            raise web.HTTPNotFound()
        try:
            async with asyncio.timeout(REQUEST_TIMEOUT_SECONDS):
                body = await _search_body(request)
                async with request.app[_SESSION].post(
                    base + '/api/scryer/search',
                    data=body,
                    headers={'Content-Type': 'application/json'},
                    allow_redirects=False,
                ) as response:
                    payload = await _bounded_json_response(response)
                return web.json_response(_safe_search_response(payload))
        except (ClientError, TimeoutError, ValueError, UnicodeError):
            return _unavailable()

    app.cleanup_ctx.append(lifetime)
    app.router.add_get(prefix + '/api/status', status)
    app.router.add_post(prefix + '/api/scryer/search', search)
