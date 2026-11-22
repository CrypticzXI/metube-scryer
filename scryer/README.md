# MeTube for Scryer

This fork of **alexta69/metube** adds a custom HTTP indexer for an unchanged Scryer release. MeTube handles pasted URLs, channel/playlist subscriptions, queues and files. Scryer manages its normal catalogue, monitored episodes, quality profiles and imports into a dedicated YouTube library. All discovery and downloads use **yt-dlp**, without a Google API key or a new access token.

```text
Scryer → YouTube Library indexer plugin → MeTube /api/scryer/search
                                       → bundled metadata worker → yt-dlp
Scryer → existing MeTube download-client plugin → MeTube queue → yt-dlp
```

The worker only reads metadata. It has no download endpoint or separate queue. It runs inside the same container, bound to loopback, and MeTube exposes its routes at the usual MeTube address. MeTube's existing UI, subscriptions and download API continue to work.

## Start the fork

From the repository root:

```sh
docker compose -f compose.scryer.yaml up --build -d
```

Open `http://localhost:8081`. Downloads are stored in `./downloads`, and persistent MeTube state (including subscriptions) in `./state`. Compose sets `DELETE_FILE_ON_TRASHCAN=false` so clearing history after a Scryer import does not delete the imported file.

The default Docker target is `metube-scryer`. Building with `--target metube` creates the plain downloader with the indexer routes disabled unless `SCRYER_INDEXER_URL` is configured. Both MeTube and the worker stop together; either process exiting restarts the container through Compose's restart policy.

## Install and route in Scryer

1. Install the custom `youtube-library.wasm` indexer in **Settings → Plugins** using manual file installation. It identifies itself as **YouTube Library**, provider `youtube-library`, HTTP family. Target Scryer 0.23 or newer.
2. Install/configure **MeTube** as the download client. The included `metube.wasm` is a minimally adapted copy of [the developer's MeTube plugin](https://github.com/scryer-media/scryer-plugins/tree/feature/metube-download-client/download_clients/metube), with published SDK compatibility. Its download behavior is unchanged; source and provenance are in `scryer/download-client`.
3. Add a YouTube Library indexer. Set **Indexer Service URL** to this MeTube instance, e.g. `http://host.docker.internal:8081` from a Scryer container on Docker Desktop, or `http://metube:8081` on a shared Docker network. A native Scryer process can use `http://127.0.0.1:8081`. Test connection.
4. Set the MeTube client's **base_url** to the same address and **download_dir** to `/downloads`. Share that directory with Scryer, adding a remote path mapping if Scryer mounts it elsewhere. Leave custom subfolders enabled in MeTube.
5. Create a dedicated **YouTube** library in Scryer's **Series** facet, with its own root and quality profile. Route only the YouTube Library indexer and MeTube client to it. Add catalogue titles through Scryer's normal flow.

Plain interactive text searches work without a source mapping. Pasted video links can be submitted directly in MeTube; an interactive URL query returns the corresponding HTTP video release for Scryer. Channel and playlist subscriptions are managed in MeTube's existing Subscriptions UI.

## Monitored Sidemen Sundays episodes

Enable **automatic searches** on the custom indexer. In **Official series sources (JSON)**, enter:

```json
[
  {
    "series_name": "Sidemen Sundays",
    "tvdb_id": "388373",
    "channel_url": "https://www.youtube.com/@Sidemen/videos"
  }
]
```

Enter a **Scryer URL for episode metadata** reachable from the MeTube container, e.g. `http://host.docker.internal:8080` for the user's Windows Scryer instance. Supply the existing Scryer username/password if login is enabled. Scryer stores the password field encrypted; the worker uses it for an in-memory login session and does not persist it. An explicit URL without credentials uses Scryer's open-admin mode.

Scryer 0.23 does not fill the episode title/date in every plugin search request. The worker resolves the exact requested season/episode from Scryer's catalogue, verifies the official channel ID, descriptive episode title, upload date and duration, then probes the selected video format. Only a unique verified match receives the catalogue `SxxEyy` marker. Playlist order is never treated as episode numbering, and unknown quality or size is never invented.

Source mappings accept the exact catalogue series name, an optional Scryer title UUID (`scryer_title_id`), an optional TVDB ID, and an official channel or playlist URL. At most 100 are accepted. A mapping alone never starts a download. Scryer's wanted searches decide which monitored episodes to acquire. MeTube subscriptions are independent: enabling one can download uploads outside the Scryer episode catalogue.

RSS discovery, season packs, automatic movie matching and absolute-only episode numbering are not implemented. Ambiguous or unverifiable matches produce no release. Availability is limited to public videos that yt-dlp can access.

## Build and validate

The Rust components are independent workspaces, GPL-3.0-only subprojects; MeTube retains its upstream AGPL license. The custom plugin uses the published Scryer SDK 3.18.0 / PDK 0.8.0 and their WASI Preview 2 component contract.

```sh
cargo test --manifest-path scryer/indexer/Cargo.toml --locked
cargo test --manifest-path scryer/service/Cargo.toml --features test-fixtures --locked
cargo build --manifest-path scryer/indexer/Cargo.toml --target wasm32-wasip2 --profile plugin-release --locked
python scryer/indexer/package.py
python scryer/indexer/package.py --component download-client
uv sync --frozen --group dev
uv run pytest app/tests/
```

`package.py` builds each WASI component, exports its descriptor and writes the installable `dist/youtube-library.wasm` or `dist/metube.wasm` with the required `scryer.plugin-descriptor.v1` custom section. Source and SDK protocol tests are separate from full Scryer-host acceptance.

For native development, run the worker with `YTDLP_BIN` pointing to yt-dlp and `YOUTUBE_LIBRARY_BIND=127.0.0.1:8090`; run MeTube with `SCRYER_INDEXER_URL=http://127.0.0.1:8090`. `YOUTUBE_MAX_HEIGHT` controls the probed resolution ceiling (default 1080). `YTDLP_JS_RUNTIME` can point to Deno or Node for YouTube challenges. None of these settings accepts arbitrary shell options.
