"""Shared helpers for guarded benchmarks, soak, and responsiveness tests.

Every browser started here is watched by tools/window_audit.swift (SIGKILL on a
visible window, frontmost activation, or a Dock icon) and by the shared
watchdog. Process accounting walks the process tree of one browser PID and
classifies Chromium helpers by their --type switches.
"""
import json
import os
import shutil
import statistics
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

import aside_api
from memory_benchmark import MacUsageV2

ROOT = Path(__file__).resolve().parent
WATCHDOG = Path('/Users/mohammadalmasalmeh/Desktop/aside-rs-wt/watchdog.sh')
AUDIT_SOURCE = ROOT / 'window_audit.swift'
AUDIT_BINARY = Path('/private/tmp/aside-perf-window-audit')
ASIDE = '/Applications/Aside.app/Contents/MacOS/Aside'
MIB = 1048576


def process_table():
    rows = subprocess.check_output(['ps', '-axww', '-o', 'pid=,ppid=,rss=,stat=,command='], text=True)
    table = {}
    for row in rows.splitlines():
        parts = row.strip().split(None, 4)
        if len(parts) == 5:
            pid, parent, rss, stat, command = parts
            # Exited children that were not reaped yet hold no memory.
            table[int(pid)] = (int(parent), int(rss), '<zombie>' if stat.startswith('Z') else command)
    return table


def descendants(table, root):
    selected = {root} & table.keys()
    while True:
        expanded = selected | {pid for pid, (parent, _, _) in table.items() if parent in selected}
        if expanded == selected:
            return selected
        selected = expanded


def classify(pid, root, command):
    if pid == root:
        return 'browser'
    if command == '<zombie>':
        return 'zombie'
    if 'aside-daemon' in command:
        return 'aside_daemon'
    if '--type=gpu-process' in command:
        return 'gpu'
    if '--type=utility' in command:
        return 'network' if 'network.mojom.NetworkService' in command else 'utility'
    if '--type=renderer' in command:
        return 'extension_renderer' if '--extension-process' in command else 'renderer'
    if 'crashpad' in command:
        return 'crashpad'
    return 'other'


_libproc = None


def footprint_bytes(pid):
    global _libproc
    if sys.platform != 'darwin':
        return None
    if _libproc is None:
        import ctypes
        _libproc = ctypes.CDLL('/usr/lib/libproc.dylib', use_errno=True)
        _libproc.proc_pid_rusage.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
        _libproc.proc_pid_rusage.restype = ctypes.c_int
    import ctypes
    usage = MacUsageV2()
    if _libproc.proc_pid_rusage(pid, 2, ctypes.byref(usage)) != 0:
        return None
    return usage.phys_footprint


def snapshot(root):
    """RSS and physical footprint of one browser's process tree, total and per process type."""
    table = process_table()
    pids = descendants(table, root)
    if not pids:
        return None
    types, total_rss, total_fp, complete, zombies = {}, 0, 0, True, 0
    for pid in pids:
        _, rss_kib, command = table[pid]
        kind = classify(pid, root, command)
        if kind == 'zombie':
            zombies += 1
            continue
        fp = footprint_bytes(pid)
        entry = types.setdefault(kind, {'count': 0, 'rss_mib': 0.0, 'footprint_mib': 0.0})
        entry['count'] += 1
        if kind == 'other':
            name = command.split(' --')[0].rsplit('/', 1)[-1]
            if name not in entry.setdefault('executables', []):
                entry['executables'].append(name)
        entry['rss_mib'] += rss_kib / 1024
        total_rss += rss_kib * 1024
        if fp is None:
            complete = False
        else:
            entry['footprint_mib'] += fp / MIB
            total_fp += fp
    for entry in types.values():
        entry['rss_mib'] = round(entry['rss_mib'], 1)
        entry['footprint_mib'] = round(entry['footprint_mib'], 1)
    return {'process_count': len(pids) - zombies, 'zombies': zombies, 'rss_mib': round(total_rss / MIB, 1),
            'footprint_mib': round(total_fp / MIB, 1) if complete else None,
            'by_type': dict(sorted(types.items()))}


def summarize_samples(samples):
    """Median of per-sample totals and per-type values over a sampling phase."""
    usable = [s for s in samples if s]
    fps = [s['footprint_mib'] for s in usable if s['footprint_mib'] is not None]
    kinds = sorted({k for s in usable for k in s['by_type']})
    med = lambda values: round(statistics.median(values), 1) if values else None
    return {
        'samples': len(usable), 'complete_footprint_samples': len(fps),
        'footprint_mib': med(fps) if len(fps) * 2 > len(usable) else None,
        'rss_mib': med([s['rss_mib'] for s in usable]),
        'process_count': med([s['process_count'] for s in usable]),
        'by_type': {k: {field: med([s['by_type'].get(k, {}).get(field, 0) for s in usable])
                        for field in ('count', 'rss_mib', 'footprint_mib')} for k in kinds},
    }


def spread(values):
    values = [v for v in values if v is not None]
    if not values:
        return None
    return {'median': round(statistics.median(values), 1), 'min': min(values), 'max': max(values), 'runs': values}


class SleepDetector:
    """The monotonic clock stops while the Mac sleeps; the wall clock does not."""

    def __init__(self):
        self.offset = time.time() - time.monotonic()

    def slept_seconds(self):
        return round(max(0.0, time.time() - time.monotonic() - self.offset), 1)


def system_state():
    load = os.getloadavg()
    level = subprocess.run(['sysctl', '-n', 'kern.memorystatus_level'], capture_output=True, text=True).stdout.strip()
    return {'load_1m': round(load[0], 1), 'load_5m': round(load[1], 1),
            'memory_free_percent': int(level) if level.isdigit() else None}


def wait_for_quiet(max_load, timeout, log=print):
    """Wait until the 1-minute load average drops below max_load; record what happened."""
    started = time.monotonic()
    while True:
        state = system_state()
        if state['load_1m'] <= max_load:
            return {**state, 'waited_seconds': round(time.monotonic() - started), 'quiet': True}
        if time.monotonic() - started >= timeout:
            return {**state, 'waited_seconds': round(time.monotonic() - started), 'quiet': False}
        log(f"waiting for a quieter system: load {state['load_1m']} > {max_load}", flush=True)
        time.sleep(15)


def frontmost_pid():
    front = subprocess.run(['lsappinfo', 'front'], capture_output=True, text=True).stdout.strip()
    out = subprocess.run(['lsappinfo', 'info', '-only', 'pid', front], capture_output=True, text=True).stdout
    digits = ''.join(c for c in out if c.isdigit())
    return int(digits) if digits else None


def audit_binary():
    if not AUDIT_BINARY.exists() or AUDIT_BINARY.stat().st_mtime < AUDIT_SOURCE.stat().st_mtime:
        subprocess.run(['swiftc', '-O', str(AUDIT_SOURCE), '-o', str(AUDIT_BINARY)], check=True)
    return str(AUDIT_BINARY)


class Guard:
    """Window audit (kills on violation) plus the shared frontmost watchdog for one PID."""

    # Bundled apps check in with LaunchServices as Regular before their code switches to
    # Prohibited (observed 50-250 ms); a longer continuous Regular state is a violation.
    def __init__(self, pid, regular_grace=1.0):
        self.pid = pid
        self.front_before = frontmost_pid()
        self.audit = subprocess.Popen([audit_binary(), str(pid), '--kill', '--regular-grace', str(regular_grace)],
                                      stdout=subprocess.PIPE, text=True)
        self.watchdog = subprocess.Popen([str(WATCHDOG), str(pid)], stdout=subprocess.PIPE, text=True)

    def finish(self):
        """Call after the process exited."""
        audit = json.loads(self.audit.communicate(timeout=15)[0] or '{}')
        watchdog = self.watchdog.communicate(timeout=15)[0].strip()
        front_after = frontmost_pid()
        timeline = audit.get('policy_timeline') or []
        regular = sum((b['t'] if b else audit.get('seconds', a['t'])) - a['t']
                      for a, b in zip(timeline, timeline[1:] + [None]) if a['policy'] == 'regular')
        return {'ok': bool(audit.get('ok')) and 'FRONTMOST' not in watchdog,
                'regular_policy_seconds': round(regular, 3),
                'test_process_frontmost_before': self.front_before == self.pid,
                'test_process_frontmost_after': front_after == self.pid,
                'audit_polls': audit.get('polls'), 'audit_interval_ms': audit.get('interval_ms'),
                'activation_policies_seen': audit.get('activation_policies_seen'),
                'policy_timeline': audit.get('policy_timeline'),
                'violations': audit.get('violations'), 'watchdog': watchdog.split(' ', 1)[-1]}


def stop_process(process, timeout=15):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


def helper_pids(root):
    table = process_table()
    return {pid: table[pid][2] for pid in descendants(table, root) if pid != root}


def reap_orphans(helpers, wait=5):
    """Kill helpers that outlived their browser; return how many there were."""
    deadline = time.monotonic() + wait
    while True:
        table = process_table()
        alive = [pid for pid, command in helpers.items() if pid in table and table[pid][0] == 1 and table[pid][2] == command]
        if not alive or time.monotonic() >= deadline:
            break
        time.sleep(.5)
    for pid in alive:
        try:
            os.kill(pid, 9)
        except ProcessLookupError:
            pass
    return len(alive)


class CefInstance:
    """A fresh, isolated, background CEF build on a loopback port."""

    def __init__(self, app, port, data_dir, extra_env=None):
        self.data_dir = Path(data_dir)
        shutil.rmtree(self.data_dir, ignore_errors=True)
        self.data_dir.mkdir(parents=True)
        self.log_path = self.data_dir / 'launch.log'
        self.client = aside_api.Client(f'http://127.0.0.1:{port}', data_dir=self.data_dir)
        self._log = self.log_path.open('w')
        self.process = subprocess.Popen(
            [str(Path(app).resolve()), '--data-dir', str(self.data_dir), 'run', '--port', str(port),
             '--opencode-url', 'http://127.0.0.1:1'],
            stdout=self._log, stderr=self._log, env={**aside_api.instance_env(), **(extra_env or {})})
        self.guard = Guard(self.process.pid)
        self.pid = self.process.pid
        for _ in range(240):
            try:
                if self.client.request('/api/browser/memory', timeout=5).get('shell_ready'):
                    break
            except (urllib.error.URLError, TimeoutError, ConnectionError):
                pass
            if self.process.poll() is not None:
                break
            time.sleep(.5)
        else:
            stop_process(self.process)
        if self.process.poll() is not None:
            self._log.close()
            raise RuntimeError(f'CEF instance did not start (exit {self.process.returncode}); guard {self.guard.finish()}; '
                               f'log {self.log_path}')

    def api(self, path, data=None, timeout=40):
        return self.client.api(path, data, timeout)

    def command(self, **data):
        return self.client.command(**data)

    def focus(self, tab_id, attempts=8):
        """Make tab_id the agent's page. switch_tab takes an index into the non-scratch tabs as the
        app sees them when the tool runs, which can differ from an earlier listing while a close
        is in flight, so the choice is confirmed against a fresh listing."""
        for _ in range(attempts):
            listing = lambda: [t['id'] for t in self.api('/api/browser/tabs')['tabs'] if t.get('kind') != 'scratch']
            ids = listing()
            if tab_id not in ids:
                raise RuntimeError(f'tab {tab_id} is not open')
            index = ids.index(tab_id)
            try:
                self.client.tool('switch_tab', {'index': index})
            except AssertionError:
                time.sleep(.5)
                continue
            if listing() == ids:
                return index
            time.sleep(.5)
        raise RuntimeError(f'could not select tab {tab_id} for the agent')

    def evaluate(self, tab_id, code):
        self.focus(tab_id)
        return json.loads(self.client.tool('evaluate', {'code': code})[0]['text'])

    def devtools(self, method, params=None, tab=None):
        result = self.client.request('/api/browser/devtools', {'tab': tab, 'method': method, 'params': params or {}}, timeout=40)
        if 'error' in result:
            raise RuntimeError(result['error'])
        return result['result']

    def stop(self):
        helpers = helper_pids(self.pid)
        try:
            self.client.request('/api/browser/command', {'cmd': 'quit'}, timeout=5)
        except (urllib.error.URLError, TimeoutError, ConnectionError):
            pass
        try:
            self.process.wait(timeout=20)
        except subprocess.TimeoutExpired:
            stop_process(self.process)
        self._log.close()
        return {**self.guard.finish(), 'helpers_outliving_browser': reap_orphans(helpers)}


def cdp(url, method, params=None, timeout=25):
    result = subprocess.run(['node', str(ROOT / 'cdp.mjs')], input=json.dumps({'url': url, 'method': method, 'params': params or {}}),
                            capture_output=True, text=True, timeout=timeout)
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip())
    return json.loads(result.stdout)


class AsideBaseline:
    """The installed Aside in Chromium's headless mode with a throwaway profile.

    Headless mode creates no windows and sets the Prohibited activation policy,
    so the user's screen, Dock, and focus are untouched. The user's own running
    Aside uses a different profile and is never contacted.
    """

    # Headless mode advertises "HeadlessChrome" and navigator.webdriver, and sites serve such
    # clients fewer ads and embeds, so the baseline presents itself like a windowed Aside.
    CHROME_MAJOR = 154
    USER_AGENT = ('Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) '
                  f'Chrome/{CHROME_MAJOR}.0.0.0 Safari/537.36')

    # Aside's sidebar and toolbar take 280x42 px; this window gives a 1024x808 page viewport.
    def __init__(self, profile, scale_factor=2, size=(1304, 850)):
        self.profile = Path(profile)
        shutil.rmtree(self.profile, ignore_errors=True)
        self.profile.mkdir(parents=True)
        self.size = size
        self._log = (self.profile / 'launch.log').open('w')
        self.process = subprocess.Popen(
            [ASIDE, '--headless', f'--user-data-dir={self.profile}', '--remote-debugging-port=0', '--no-first-run',
             '--no-default-browser-check', '--use-mock-keychain', f'--window-size={size[0]},{size[1]}',
             f'--force-device-scale-factor={scale_factor}', f'--user-agent={self.USER_AGENT}',
             '--disable-blink-features=AutomationControlled'],
            stdout=self._log, stderr=self._log)
        self.guard = Guard(self.process.pid)
        self.pid = self.process.pid
        port_file = self.profile / 'DevToolsActivePort'
        for _ in range(120):
            if port_file.exists() and port_file.read_text().strip():
                break
            if self.process.poll() is not None:
                break
            time.sleep(.5)
        else:
            stop_process(self.process)
        if self.process.poll() is not None:
            self._log.close()
            raise RuntimeError(f'Aside baseline did not start (exit {self.process.returncode}); guard {self.guard.finish()}')
        self.port = int(port_file.read_text().splitlines()[0])
        self.version = self.json('/json/version')
        if not self.version.get('Browser', '').startswith(f'Chrome/{self.CHROME_MAJOR}.'):
            self.stop()
            raise RuntimeError(f'Aside is now {self.version.get("Browser")}; update AsideBaseline.CHROME_MAJOR')
        # Tabs created before Aside's startup window exists can be dropped; wait for its first page.
        for _ in range(60):
            if any(t.get('type') == 'page' for t in self.json('/json/list')):
                break
            time.sleep(.5)
        time.sleep(2)

    def json(self, path):
        with urllib.request.urlopen(f'http://127.0.0.1:{self.port}{path}', timeout=10) as response:
            return json.load(response)

    def open(self, url):
        return cdp(self.version['webSocketDebuggerUrl'], 'Target.createTarget', {'url': url})['targetId']

    def close_other_pages(self, keep):
        """Close Aside's own new-tab page so only the workload's pages remain."""
        for target in self.json('/json/list'):
            if target.get('type') == 'page' and target['id'] not in keep:
                self.close(target['id'])

    def close(self, target_id):
        try:
            cdp(self.version['webSocketDebuggerUrl'], 'Target.closeTarget', {'targetId': target_id})
        except RuntimeError as error:
            if 'No target with given id' not in str(error):
                raise

    def target(self, target_id):
        return next((t for t in self.json('/json/list') if t.get('id') == target_id), None)

    replaced = 0

    def resolve(self, target_id, url):
        """Aside sometimes replaces a new tab's target while it navigates; follow it by URL."""
        if self.target(target_id):
            return target_id
        match = next((t['id'] for t in self.json('/json/list') if t.get('type') == 'page' and t.get('url') == url), None)
        if match:
            self.replaced += 1
        return match or target_id

    def evaluate(self, target_id, expression):
        target = self.target(target_id)
        if not target:
            raise RuntimeError('baseline target is gone')
        result = cdp(target['webSocketDebuggerUrl'], 'Runtime.evaluate',
                     {'expression': expression, 'returnByValue': True, 'awaitPromise': True})
        if 'exceptionDetails' in result:
            raise RuntimeError(str(result['exceptionDetails'])[:300])
        return result['result'].get('value')

    def stop(self):
        helpers = helper_pids(self.pid)
        stop_process(self.process)
        self._log.close()
        return {**self.guard.finish(), 'helpers_outliving_browser': reap_orphans(helpers)}


class FixtureServer:
    def __init__(self, port):
        self.url = f'http://127.0.0.1:{port}'
        self.process = subprocess.Popen([sys.executable, str(ROOT / 'fixture_server.py'), '--port', str(port)],
                                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        # The first start may encode the video clip with ffmpeg.
        for _ in range(450):
            try:
                urllib.request.urlopen(self.url + '/page/ready', timeout=2).read()
                return
            except (urllib.error.URLError, ConnectionError):
                if self.process.poll() is not None:
                    break
                time.sleep(.2)
        stop_process(self.process, 5)
        raise RuntimeError('fixture server did not start')

    def stop(self):
        stop_process(self.process, 5)
