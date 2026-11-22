"""Run MeTube and its read-only indexer as one container lifecycle."""
import os
import signal
import subprocess
import time


def terminate(children):
    """Stop each whole process group, including extractor/helper children."""
    for child in children:
        if child.poll() is None:
            try:
                os.killpg(child.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
    deadline = time.monotonic() + 10
    for child in children:
        try:
            child.wait(timeout=max(0.01, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()


def supervise(commands):
    children = []
    stopping = False

    def stop(_signum, _frame):
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    try:
        for command in commands:
            children.append(subprocess.Popen(command, start_new_session=True))
        while not stopping:
            for child in children:
                status = child.poll()
                if status is not None:
                    # Any unexpected exit restarts the entire container, even exit 0.
                    return status or 1
            time.sleep(0.2)
        return 0
    finally:
        terminate(children)


if __name__ == '__main__':
    worker = ['/usr/local/bin/scryer-youtube-library-service']
    if os.geteuid() == 0:
        uid = os.environ.get('UID', os.environ.get('PUID', '1000'))
        gid = os.environ.get('GID', os.environ.get('PGID', '1000'))
        worker = ['gosu', f'{uid}:{gid}', *worker]
    raise SystemExit(supervise([worker, ['/app/docker-entrypoint.sh']]))
