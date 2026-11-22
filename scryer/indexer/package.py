"""Build and package an installable Scryer WASI plugin (GPL-3.0-only)."""
import argparse
import json
import os
from pathlib import Path
import subprocess


def leb(value):
    output = bytearray()
    while True:
        byte = value & 127
        value >>= 7
        output.append(byte | (128 if value else 0))
        if not value:
            return bytes(output)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--component', choices=['indexer', 'download-client'], default='indexer')
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    crate = Path(__file__).resolve().parent.parent / args.component
    plugin_id = 'youtube-library' if args.component == 'indexer' else 'metube'
    crate_name = 'youtube_library_indexer' if args.component == 'indexer' else 'metube_download_client'
    example = 'indexer-descriptor' if args.component == 'indexer' else 'download-client-descriptor'
    destination = args.output or Path('dist') / (plugin_id + '.wasm')
    manifest = crate / 'Cargo.toml'
    subprocess.run(['cargo', 'build', '--manifest-path', str(manifest), '--locked',
                    '--target', 'wasm32-wasip2', '--profile', 'plugin-release'], check=True)
    descriptor = subprocess.check_output(['cargo', 'run', '--quiet', '--manifest-path',
                                         str(manifest), '--example', example, '--locked'])
    target = Path(os.environ.get('CARGO_TARGET_DIR', crate / 'target'))
    wasm = (target / 'wasm32-wasip2/plugin-release' / (crate_name + '.wasm')).read_bytes()
    if wasm[:8] != b'\x00asm\x0d\x00\x01\x00':
        raise ValueError('Expected a WASI Preview 2 component')
    document = json.loads(descriptor)
    if document['id'] != plugin_id:
        raise ValueError('Unexpected plugin descriptor')
    name = b'scryer.plugin-descriptor.v1'
    body = leb(len(name)) + name + json.dumps(document, separators=(',', ':'), ensure_ascii=False).encode()
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(wasm + b'\x00' + leb(len(body)) + body)
    print(f'Packaged {document["id"]} {document["version"]}: {destination}')


if __name__ == '__main__':
    main()
