# MeTube for Scryer — Windows

Extract the ZIP, then double-click **Start-MeTube.cmd**. Keep its console window open while using MeTube. It opens `http://127.0.0.1:8081` when ready; Ctrl+C stops MeTube and its indexer worker.

Python, yt-dlp, Deno, FFmpeg and ffprobe are bundled. No Docker, Python installation, Google API key or new access token is required. Downloads, subscriptions and state are kept beside the launcher in `downloads`, `state` and `temp`. Keep these folders when updating.

Install **plugins/youtube-library.wasm** and **plugins/metube.wasm** in Scryer. Set both plugin addresses to this MeTube instance. For a native Scryer process, use `http://127.0.0.1:8081`; for Scryer in Docker Desktop, use `http://host.docker.internal:8081` and set `HOST=0.0.0.0` before starting if necessary. Set the MeTube client's download directory to the absolute Windows `downloads` folder; add a remote path mapping when Scryer sees that shared directory elsewhere.

See **SCRYER-SETUP.md** for the dedicated YouTube library, official channel mappings and monitored episode setup. MeTube's own UI manages pasted links and channel/playlist subscriptions.

Environment variables such as `PORT`, `HOST`, `DOWNLOAD_DIR`, `STATE_DIR` and `TEMP_DIR` override the defaults. The metadata worker uses `YOUTUBE_LIBRARY_BIND` (default `127.0.0.1:8090`) and MeTube connects through `SCRYER_INDEXER_URL`. Change both together if changing its port.

This is a portable x64 build. The source commit is recorded in **SOURCE-COMMIT.txt** and bundled file hashes in **SHA256SUMS**. MeTube retains its AGPL license; the Scryer plugin components retain their GPL licenses. Third-party runtime licenses are included with their files.
