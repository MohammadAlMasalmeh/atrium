"""Isolated, never-frontmost copies of the app bundle for release tests.

Copies get LSBackgroundOnly in Info.plist, so LaunchServices registers them as
background-only from the moment they check in (the plist form of the Prohibited
activation policy), and are re-signed ad hoc. Bundles with a quarantine xattr are
never started, because Gatekeeper would show a dialog for them. Every launch also uses
ATRIUM_MOCK_KEYCHAIN=1 and ATRIUM_BACKGROUND_WINDOW=1. A FrontWatcher kills any
process running from the test directory that becomes the frontmost app and
fails the test; the shared watchdog script is also attached to each known PID.
"""
import os
import plistlib
import shutil
import subprocess
import threading
import time
import urllib.error
from pathlib import Path

import atrium_api

TOOLS = Path(__file__).resolve().parent
WATCHDOG = Path(os.environ.get('ATRIUM_WATCHDOG', '/Users/mohammadalmasalmeh/Desktop/atrium-rs-wt/watchdog.sh'))


def run(*command, **kwargs):
    return subprocess.run([str(c) for c in command], check=True, capture_output=True, text=True, **kwargs)


def info(app, key):
    with open(Path(app) / 'Contents/Info.plist', 'rb') as f:
        return plistlib.load(f).get(key)


def prepare(source, dest, version=None, executable=None):
    """Copy a bundle, make it background-only, optionally change its version or main executable, and sign it."""
    dest = Path(dest)
    if dest.exists():
        shutil.rmtree(dest)
    dest.parent.mkdir(parents=True, exist_ok=True)
    run('/usr/bin/ditto', source, dest)
    plist = dest / 'Contents/Info.plist'
    run('/usr/bin/plutil', '-replace', 'LSBackgroundOnly', '-bool', 'YES', plist)
    if version:
        run('/usr/bin/plutil', '-replace', 'CFBundleVersion', '-string', version, plist)
        run('/usr/bin/plutil', '-replace', 'CFBundleShortVersionString', '-string', version.split('-')[0], plist)
    if executable:
        target = dest / 'Contents/MacOS' / info(dest, 'CFBundleExecutable')
        shutil.copyfile(executable, target)
        target.chmod(0o755)
    env = {k: v for k, v in os.environ.items() if k not in ('ATRIUM_SIGN_IDENTITY', 'ATRIUM_HARDENED_RUNTIME')}
    run(TOOLS / 'sign-app.sh', dest, env=env)
    assert info(dest, 'LSBackgroundOnly') is True
    return dest


class FrontWatcher:
    """Kills and records any process under `root` that becomes the frontmost application."""

    def __init__(self, root):
        self.root = str(Path(root).resolve())
        self.violations = []
        self.stopped = threading.Event()
        self.thread = threading.Thread(target=self.loop, daemon=True)
        self.thread.start()

    def loop(self):
        while not self.stopped.is_set():
            try:
                front = subprocess.run(['lsappinfo', 'front'], capture_output=True, text=True, timeout=5).stdout.strip()
                out = subprocess.run(['lsappinfo', 'info', '-only', 'pid', front], capture_output=True, text=True, timeout=5).stdout
                pid = ''.join(ch for ch in out if ch.isdigit())
                if pid:
                    path = subprocess.run(['ps', '-o', 'comm=', '-p', pid], capture_output=True, text=True, timeout=5).stdout.strip()
                    if path.startswith(self.root) or path.startswith('/private' + self.root):
                        subprocess.run(['kill', '-9', pid])
                        self.violations.append(f'{time.strftime("%H:%M:%S")} pid {pid} {path}')
            except Exception:
                pass
            time.sleep(0.3)

    def stop(self):
        self.stopped.set()
        self.thread.join(timeout=5)

    def check(self):
        assert not self.violations, f'A test instance became frontmost and was killed: {self.violations}'


def watchdog(pid, log_dir):
    if WATCHDOG.exists():
        log = open(Path(log_dir) / f'watchdog-{pid}.log', 'w')
        subprocess.Popen(['/bin/sh', str(WATCHDOG), str(pid)], stdout=log, stderr=log, start_new_session=True)


class Instance:
    """One background browser started from a prepared bundle."""

    def __init__(self, app, data_dir, port, log, extra_env=None):
        self.app, self.data_dir, self.port = Path(app), Path(data_dir), port
        self.client = atrium_api.Client(f'http://127.0.0.1:{port}', data_dir=self.data_dir)
        self.log = open(log, 'a')
        self.extra_env = extra_env or {}
        self.process = None

    def command(self):
        exe = self.app / 'Contents/MacOS' / info(self.app, 'CFBundleExecutable')
        return [str(exe), '--data-dir', str(self.data_dir), 'run', '--port', str(self.port), '--opencode-url', 'http://127.0.0.1:1']

    def start(self):
        xattrs = subprocess.run(['/usr/bin/xattr', '-r', '-l', str(self.app)], capture_output=True, text=True).stdout
        assert 'com.apple.quarantine' not in xattrs, f'refusing to start quarantined code (Gatekeeper would show a dialog): {self.app}'
        self.process = subprocess.Popen(self.command(), stdout=self.log, stderr=self.log, env={**atrium_api.instance_env(), **self.extra_env})
        watchdog(self.process.pid, self.data_dir.parent)
        return self.process

    def wait_ready(self, timeout=90):
        """Waits for a loaded shell; returns the PID that answers the API and attaches a watchdog to it."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                memory = self.client.api('/api/browser/memory', timeout=5)
                if memory.get('shell_ready'):
                    if not self.process or memory['pid'] != self.process.pid:
                        watchdog(memory['pid'], self.data_dir.parent)
                    return memory['pid']
            except (urllib.error.URLError, TimeoutError, ConnectionError, AssertionError):
                pass
            time.sleep(0.5)
        raise AssertionError(f'The browser on port {self.port} did not become ready')

    def quit(self, pid, timeout=30):
        try:
            self.client.api('/api/browser/command', {'cmd': 'quit'}, timeout=5)
        except (urllib.error.URLError, TimeoutError, ConnectionError, AssertionError):
            pass
        if self.process and self.process.pid == pid:
            self.process.wait(timeout)
        else:
            wait_exit(pid, timeout)

    def kill_all(self):
        """Last-resort cleanup of every process started from this test's bundles."""
        subprocess.run(['pkill', '-9', '-f', str(self.app.parent)], capture_output=True)


def alive(pid):
    try:
        os.waitpid(pid, os.WNOHANG)
    except ChildProcessError:
        pass
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def wait_exit(pid, timeout=30):
    deadline = time.time() + timeout
    while alive(pid) and time.time() < deadline:
        time.sleep(0.2)
    if alive(pid):
        os.kill(pid, 9)
        raise AssertionError(f'process {pid} did not exit in {timeout}s')
