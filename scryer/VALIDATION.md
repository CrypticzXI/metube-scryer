# Validation

Validated on Windows 11 with Rust 1.98.1, and with Docker Desktop's Linux engine.

- Custom indexer: 11 Rust tests passed; WASI Preview 2 component built and packaged with its descriptor.
- Vendored MeTube download client: all 34 original inline tests passed with the SDK compatibility patch; WASI component built and packaged.
- Metadata worker: 16 unit tests and one endpoint test with a real fixture subprocess passed.
- MeTube proxy: all 23 focused aiohttp tests passed.
- Complete MeTube backend suite in the Linux image: **909 passed, 1 failed**. The failure is `WatchOptionsFileTests::test_reloads_after_every_atomic_replace`, which also reproduces when the container runs the unmodified upstream `app/main.py`. The indexer does not change that file-watcher implementation. The Windows run had additional Unix signal/permission-dependent failures (890 passed, 19 failed, 1 skipped).
- Fork Docker image built successfully, including MeTube's Angular production build and the Rust worker.
- Container `/version`, `/history`, and indexer `/api/status` returned successfully, with no queued download.
- Read-only live lookup through the combined MeTube endpoint resolved Sidemen Sundays **S2026E45** to `youtube:BcOP1ZEOTo0`, the official **SIDEMEN YOUTUBER PUB QUIZ** upload, with a real 1080p/AV1 selected format. The request completed in about nine seconds. No media was downloaded.
- Stopping the test worker caused its MeTube container to exit and clean up, verifying the shared lifecycle.

These checks do not claim that the new plugin has been installed or that automatic acquisition/import has been exercised in the user's running Scryer instance. The existing MeTube download-client plugin remains the acquisition adapter.
