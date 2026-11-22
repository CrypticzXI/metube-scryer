# YouTube Library HTTP indexer

This third-party plugin makes the operator's yt-dlp service an HTTP indexer in standard Scryer. It returns YouTube provider-page releases to the existing **MeTube** download-client plugin. It does not replace MeTube, modify Scryer, or require a Google API key.

The WASI plugin cannot spawn yt-dlp. The bundled worker under [`scryer/service`](../service) performs searches and verifies official uploads through MeTube's HTTP routes. MeTube resolves the selected watch page and downloads the video separately. See the [fork setup guide](../README.md).

```text
Scryer → YouTube Library WASI indexer → yt-dlp indexer service
       ← verified HTTP provider-page release
Scryer → existing MeTube plugin → MeTube/yt-dlp → library import
```

## Configure

1. Run the indexer service as described in its README. Install `youtube-library.wasm` through Scryer's manual plugin installation. Add **YouTube Library** under Settings → Indexers.
2. Set **Indexer Service URL** to this fork's MeTube address reachable from Scryer, for example `http://127.0.0.1:8081`. In containers, use the service hostname instead. Test connection checks `/api/status` and requires working yt-dlp.
3. Configure the existing **MeTube** download client and its shared download directory/remote path mapping. Give the YouTube library its own indexer and download-client routing so other libraries do not search YouTube.
4. For automatic episode matching, enable automatic search on this indexer and enter **Official series sources (JSON)**, for example:

```json
[
  {
    "series_name": "Sidemen Sundays",
    "tvdb_id": "388373",
    "channel_url": "https://www.youtube.com/@Sidemen"
  }
]
```

`series_name` is the exact catalogue series name. `channel_url` must be its official HTTPS YouTube channel or playlist. An optional `scryer_title_id` can specify the Scryer title UUID. `tvdb_id` is an optional numeric string. At most 100 mappings and 64 KiB of JSON are accepted. A video link is not a source collection.

5. On Scryer 0.23, configure **Scryer URL for episode metadata** to an address the indexer service can reach. If login is enabled, also supply your existing Scryer username and password; Scryer encrypts password fields. The password is sent only to your configured indexer service for read-only catalogue lookup and is not stored by this plugin. A URL without a password selects open-admin mode. With a password, omitted URL/username default to `http://127.0.0.1:8080` and `admin`. These are Scryer credentials, not an extra service access token.

Ordinary interactive text searches work with only the indexer service URL. Automatic searches require a unique official source and verified Scryer episode metadata. The service checks real episode titles, channel identity, dates and duration; playlist order never becomes episode numbering. Ambiguous or unverified matches return no release. RSS is disabled. This plugin does not manage subscriptions or download pasted links itself; those remain separate service or MeTube operations.

## Service contract

The plugin only contacts its configured indexer service origin. Connection probes use `GET /api/status`; searches use `POST /api/scryer/search` with this envelope:

```json
{
  "request": { "query": "Sidemen Sundays", "season": 2026, "episode": 47, "limit": 20 },
  "metadata_connection": { "url": "http://scryer:8080" },
  "sources": [{ "series_name": "Sidemen Sundays", "tvdb_id": "388373", "channel_url": "https://www.youtube.com/@Sidemen" }]
}
```

`metadata_connection` is omitted when unconfigured. `sources` is always an array, including when empty. The complete SDK search request is preserved. The response is an SDK `PluginResult<PluginSearchResponse>`, with `http.provider` equal to `youtube` and a `provider_page` watch URL compatible with MeTube. Media URLs, resolution, codec and sizes must never be invented from a search listing.

## Build and test

This crate intentionally has its own workspace and pins published SDK `3.18.0` / PDK `0.8.0`; the fork's PDK does not change. From this directory with an installed Rust toolchain and WASI Preview 2 target:

```sh
cargo test --locked
cargo build --locked --target wasm32-wasip2 --profile plugin-release
cargo run --locked --example indexer-descriptor
```

From the repository root, run `python scryer/indexer/package.py` to produce the installable `dist/youtube-library.wasm`, including its descriptor custom section. The descriptor example also prints JSON for inspection.

## License and provenance

This adapter is GPL-3.0-only; see [LICENSE](LICENSE). Its Scryer component and HTTP descriptor conventions follow the [Scryer plugin SDK/PDK](https://github.com/scryer-media/scryer-plugins) and the YouTube provider-page integration developed on [`feature/metube-download-client`](https://github.com/scryer-media/scryer-plugins/tree/feature/metube-download-client). This custom adapter uses the separate yt-dlp indexer service instead of that branch's Google Data API implementation. It does not modify the upstream MeTube download-client plugin or the fork's PDK.
