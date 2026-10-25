#!/usr/bin/env python3
"""Windowed, real-site comparison of Google Chrome, the installed Aside, and this build.

Each browser runs alone with a fresh profile and a visible, frontmost 1280x820
window, the way a person would use it. Ten public sites are opened one at a time,
each in a new foreground tab, and the page's own performance timeline gives the
load times (TTFB, first contentful paint, DOMContentLoaded, load, largest
contentful paint), so harness overhead does not count. Memory is the summed
physical footprint (what Activity Monitor calls Memory) and RSS of the browser's
process tree, plus WebKit XPC processes that appear while it runs (this build's
shell is a WKWebView, whose processes are children of launchd, not of the app).

Memory is sampled 60 s after the last site loaded (all ten tabs recently used)
and again after 6 minutes idle, when each browser's own tab discarding (Chrome
Memory Saver, this build's eviction) may have acted. Finally the first three tabs
are selected again and timed until they are visible and loaded; for a discarded
tab this includes the reload.

This takes over the screen: each browser is activated and must be frontmost with an
on-screen window before every site. Run it with no other browsers open.
"""
import argparse
import json
import os
import shutil
import statistics
import subprocess
import time
import urllib.error
import urllib.request
from datetime import date
from pathlib import Path
from urllib.parse import urlparse

import atrium_api
from bench_common import cdp, footprint_bytes, process_table, descendants, classify, stop_process, system_state, MIB

ROOT = Path(__file__).resolve().parent
parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
parser.add_argument('--runs', type=int, default=3)
parser.add_argument('--browsers', default='chrome,aside,atrium')
parser.add_argument('--app', type=Path, default=ROOT.parent / 'target/bundle/atrium.app/Contents/MacOS/atrium')
parser.add_argument('--port', type=int, default=7881)
parser.add_argument('--sites', type=Path, default=ROOT / 'real_sites.json')
parser.add_argument('--live-at', type=int, default=60)
parser.add_argument('--idle-at', type=int, default=360)
parser.add_argument('--output', type=Path, required=True)
args = parser.parse_args()

CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
ASIDE = '/Applications/Aside.app/Contents/MacOS/Aside'
WINDOW = (1280, 820)  # Atrium's default window size
SCRATCH = Path('/private/tmp/atrium-three-way')

# Times are relative to the navigation's start (performance.timeOrigin).
TIMING = """(async () => {
  const nav = performance.getEntriesByType('navigation')[0];
  const fcp = performance.getEntriesByName('first-contentful-paint')[0];
  let lcp = null;
  try {
    await new Promise(resolve => {
      new PerformanceObserver(list => { for (const e of list.getEntries()) lcp = Math.max(lcp || 0, e.startTime); })
        .observe({type: 'largest-contentful-paint', buffered: true});
      setTimeout(resolve, 30);
    });
  } catch (e) {}
  const r = v => v == null ? null : Math.round(v);
  return {origin: Math.round(performance.timeOrigin), url: location.href, title: document.title, ready: document.readyState, visibility: document.visibilityState,
    textLength: document.body?.innerText.length || 0, nodes: document.querySelectorAll('*').length,
    ttfb: r(nav?.responseStart), dcl: r(nav?.domContentLoadedEventEnd), load: nav?.loadEventEnd ? r(nav.loadEventEnd) : null,
    fcp: r(fcp?.startTime), lcp: r(lcp), transfer_kib: nav ? Math.round(performance.getEntriesByType('resource')
      .reduce((s, e) => s + (e.transferSize || 0), nav.transferSize || 0) / 1024) : null,
    resources: performance.getEntriesByType('resource').length, viewport: [innerWidth, innerHeight]};
})()"""
VISIBLE = ("({origin: Math.round(performance.timeOrigin), ready: document.readyState, visibility: document.visibilityState, "
           "textLength: document.body?.innerText.length || 0, load: Math.round(performance.getEntriesByType('navigation')[0]?.loadEventEnd || 0)})")


def log(*items):
    print(time.strftime('%H:%M:%S'), *items, flush=True)


FRONT_SOURCE = ROOT / 'front_window.swift'
FRONT_BINARY = Path('/private/tmp/atrium-front-window')


def front(pid, attempts=5):
    """Activate the browser and confirm it is frontmost with an on-screen window."""
    if not FRONT_BINARY.exists() or FRONT_BINARY.stat().st_mtime < FRONT_SOURCE.stat().st_mtime:
        subprocess.run(['swiftc', '-O', str(FRONT_SOURCE), '-o', str(FRONT_BINARY)], check=True)
    for _ in range(attempts):
        state = json.loads(subprocess.run([str(FRONT_BINARY), str(pid)], capture_output=True, text=True, timeout=15).stdout)
        if state['frontmost'] and state['window']:
            return state
        time.sleep(1)
    raise RuntimeError(f'browser {pid} is not frontmost with a visible window: {state}')


def webkit_pids():
    return {pid for pid, (_, _, cmd) in process_table().items() if 'com.apple.WebKit' in cmd}


def memory(root, webkit_before):
    """Footprint and RSS of the process tree plus WebKit processes started during this run."""
    table = process_table()
    tree = descendants(table, root)
    extra = {pid for pid, (_, _, cmd) in table.items() if 'com.apple.WebKit' in cmd} - webkit_before - tree
    total_fp, total_rss, types = 0, 0, {}
    for pid in tree | extra:
        _, rss, cmd = table[pid]
        if cmd == '<zombie>':
            continue
        kind = 'webkit' if pid in extra else classify(pid, root, cmd)
        fp = footprint_bytes(pid) or 0
        entry = types.setdefault(kind, {'count': 0, 'footprint_mib': 0.0, 'rss_mib': 0.0})
        entry['count'] += 1
        entry['footprint_mib'] = round(entry['footprint_mib'] + fp / MIB, 1)
        entry['rss_mib'] = round(entry['rss_mib'] + rss / 1024, 1)
        total_fp += fp
        total_rss += rss * 1024
    return {'footprint_mib': round(total_fp / MIB, 1), 'rss_mib': round(total_rss / MIB, 1),
            'processes': sum(t['count'] for t in types.values()), 'by_type': dict(sorted(types.items()))}


def sample(root, webkit_before, count=6, every=3):
    samples = []
    for i in range(count):
        samples.append(memory(root, webkit_before))
        if i + 1 < count:
            time.sleep(every)
    med = lambda k: round(statistics.median(s[k] for s in samples), 1)
    kinds = sorted({k for s in samples for k in s['by_type']})
    return {'footprint_mib': med('footprint_mib'), 'rss_mib': med('rss_mib'), 'processes': med('processes'),
            'by_type': {k: {f: round(statistics.median(s['by_type'].get(k, {}).get(f, 0) for s in samples), 1)
                            for f in ('count', 'footprint_mib')} for k in kinds}}


class Chromium:
    """Chrome or Aside, windowed, with a throwaway profile and DevTools on a random port."""

    def __init__(self, name, binary, profile):
        self.name = name
        self.profile = Path(profile)
        shutil.rmtree(self.profile, ignore_errors=True)
        self.profile.mkdir(parents=True)
        self.webkit_before = webkit_pids()
        started = time.monotonic()
        self._log = (self.profile / 'launch.log').open('w')
        self.process = subprocess.Popen(
            [binary, f'--user-data-dir={self.profile}', '--remote-debugging-port=0', '--no-first-run',
             '--no-default-browser-check', '--use-mock-keychain', f'--window-size={WINDOW[0]},{WINDOW[1]}',
             '--window-position=40,40'], stdout=self._log, stderr=self._log)
        self.pid = self.process.pid
        port_file = self.profile / 'DevToolsActivePort'
        for _ in range(240):
            if port_file.exists() and port_file.read_text().strip():
                break
            if self.process.poll() is not None:
                raise RuntimeError(f'{name} exited during startup')
            time.sleep(.25)
        self.port = int(port_file.read_text().splitlines()[0])
        self.version = self.json('/json/version')
        for _ in range(120):
            if any(t.get('type') == 'page' for t in self.json('/json/list')):
                break
            time.sleep(.25)
        self.startup_s = round(time.monotonic() - started, 2)
        self.initial = {t['id'] for t in self.json('/json/list') if t.get('type') == 'page'}

    def json(self, path):
        with urllib.request.urlopen(f'http://127.0.0.1:{self.port}{path}', timeout=10) as response:
            return json.load(response)

    def browser(self, method, params=None):
        return cdp(self.version['webSocketDebuggerUrl'], method, params)

    def open(self, url):
        target = self.browser('Target.createTarget', {'url': url})['targetId']
        self.browser('Target.activateTarget', {'targetId': target})
        return target

    def close_initial(self):
        for target in self.initial:
            try:
                self.browser('Target.closeTarget', {'targetId': target})
            except RuntimeError:
                pass

    def activate(self, handle):
        self.browser('Target.activateTarget', {'targetId': handle})

    def evaluate(self, handle, expression):
        target = next((t for t in self.json('/json/list') if t.get('id') == handle), None)
        if not target:
            raise RuntimeError('target is gone')
        result = cdp(target['webSocketDebuggerUrl'], 'Runtime.evaluate',
                     {'expression': expression, 'returnByValue': True, 'awaitPromise': True}, timeout=30)
        if 'exceptionDetails' in result:
            raise RuntimeError(str(result['exceptionDetails'])[:200])
        return result['result'].get('value')

    def tabs(self):
        return sum(t.get('type') == 'page' for t in self.json('/json/list'))

    def stop(self):
        stop_process(self.process)
        self._log.close()


class Atrium:
    """This repository's release bundle, windowed, with a fresh data directory."""

    name = 'Atrium'

    def __init__(self, app, port, data_dir):
        self.data_dir = Path(data_dir)
        shutil.rmtree(self.data_dir, ignore_errors=True)
        self.data_dir.mkdir(parents=True)
        self.client = atrium_api.Client(f'http://127.0.0.1:{port}', data_dir=self.data_dir)
        self.webkit_before = webkit_pids()
        # Test-instance flags only allow DevTools reads and shell scripting; no background window.
        env = {**os.environ, 'ATRIUM_MOCK_KEYCHAIN': '1', 'ATRIUM_DEBUG_API': '1', 'ATRIUM_TEST_SHELL_EVAL': '1'}
        env.pop('ATRIUM_BACKGROUND_WINDOW', None)
        started = time.monotonic()
        self._log = (self.data_dir.parent / f'{self.data_dir.name}.log').open('w')
        self.process = subprocess.Popen([str(app), '--data-dir', str(self.data_dir), 'run', '--port', str(port),
                                         '--opencode-url', 'http://127.0.0.1:1'], env=env, stdout=self._log, stderr=self._log)
        self.pid = self.process.pid
        for _ in range(240):
            try:
                if self.client.request('/api/browser/memory', timeout=5).get('shell_ready') and \
                        self.client.request('/api/browser/tabs', timeout=5).get('tabs'):
                    break
            except (urllib.error.URLError, TimeoutError, ConnectionError):
                pass
            if self.process.poll() is not None:
                raise RuntimeError('Atrium exited during startup')
            time.sleep(.25)
        self.startup_s = round(time.monotonic() - started, 2)
        # A fresh profile opens onboarding over the pages; close it as its Skip button does.
        self.client.request('/api/debug/shell', 'window.atriumOnboarding && window.atriumOnboarding.close(); return 1', raw=True)
        self.initial = {t['id'] for t in self.client.api('/api/browser/tabs')['tabs']}
        self.version = {'Browser': 'Atrium (this repository, release)'}

    def open(self, url):
        before = {t['id'] for t in self.client.api('/api/browser/tabs')['tabs']}
        self.client.command(cmd='newTab', url=url, background=False)
        for _ in range(40):
            new = [t['id'] for t in self.client.api('/api/browser/tabs')['tabs'] if t['id'] not in before]
            if new:
                return new[0]
            time.sleep(.1)
        raise RuntimeError('new tab did not appear')

    def close_initial(self):
        for tab in self.initial:
            self.client.command(cmd='close', id=tab)

    def activate(self, handle):
        self.client.command(cmd='activate', id=handle)

    def evaluate(self, handle, expression):
        result = self.client.api('/api/debug/cdp', {'tab': handle, 'method': 'Runtime.evaluate', 'params': {
            'expression': expression, 'returnByValue': True, 'awaitPromise': True}}, timeout=30)['result']
        if 'exceptionDetails' in result:
            raise RuntimeError(str(result['exceptionDetails'])[:200])
        return result['result'].get('value')

    def tabs(self):
        return len(self.client.api('/api/browser/tabs')['tabs'])

    def memory_api(self):
        try:
            m = self.client.api('/api/browser/memory')
            return {'live_renderers': m.get('live_renderers'), 'not_live': sum(1 for t in m.get('tabs', []) if not t.get('live'))}
        except Exception as error:  # informational only
            return {'error': str(error)[:120]}

    def stop(self):
        try:
            self.client.request('/api/browser/command', {'cmd': 'quit'}, timeout=5)
        except (urllib.error.URLError, TimeoutError, ConnectionError):
            pass
        try:
            self.process.wait(timeout=20)
        except subprocess.TimeoutExpired:
            stop_process(self.process)
        self._log.close()


def blocked(info, site):
    if not isinstance(info, dict):
        return str(info)[:120]
    hosts = [urlparse(site['url']).hostname, *site.get('redirect_hosts', [])]
    if urlparse(info.get('url', '')).hostname not in hosts:
        return 'redirected elsewhere'
    if any(p in info.get('title', '').lower() for p in ['just a moment', 'access denied', 'security check', 'too many requests']):
        return 'challenge page'
    if info.get('textLength', 0) < 80:
        return 'little rendered content'
    return None


def load_site(b, site, timeout=30):
    started = time.monotonic()
    handle = b.open(site['url'])
    info, complete_at = None, None
    while time.monotonic() - started < timeout:
        try:
            info = b.evaluate(handle, TIMING)
        except Exception as error:
            info = str(error)[:200]
        if isinstance(info, dict) and info.get('ready') == 'complete' and info.get('load') is not None:
            complete_at = complete_at or time.monotonic()
            # Give LCP a moment to settle after the load event, as a person reading the page would.
            if time.monotonic() - complete_at >= 2:
                break
        time.sleep(.25)
    if isinstance(info, dict):
        info = {**info, 'url': urlparse(info['url'])._replace(query='', fragment='').geturl()}
    return handle, {'name': site['name'], 'timed_out': complete_at is None, 'problem': blocked(info, site), 'page': info}


def round_trip(b, handle, count=5):
    """Median time of one evaluation, so harness overhead can be taken out of switch times."""
    times = []
    for _ in range(count):
        started = time.monotonic()
        b.evaluate(handle, '1')
        times.append(time.monotonic() - started)
    return statistics.median(times)


def wait_visible(b, handle, origin, timeout=30):
    """Select a tab and wait until it is visible and loaded; report whether it had to reload."""
    started = time.monotonic()
    b.activate(handle)
    last = None
    while time.monotonic() - started < timeout:
        try:
            last = b.evaluate(handle, VISIBLE)
            if last['visibility'] == 'visible' and last['ready'] == 'complete' and last['textLength'] > 80:
                return round(time.monotonic() - started, 2), {**last, 'reloaded': last['origin'] != origin}
        except Exception as error:
            last = str(error)[:120]
        time.sleep(.05)
    return None, last


def run_browser(which, run):
    if which == 'chrome':
        b = Chromium('Google Chrome', CHROME, SCRATCH / f'chrome-{run}')
    elif which == 'aside':
        b = Chromium('Aside', ASIDE, SCRATCH / f'aside-{run}')
    else:
        b = Atrium(args.app, args.port, SCRATCH / f'atrium-{run}')
    result = {'browser': b.name, 'version': b.version.get('Browser'), 'startup_s': b.startup_s, 'system_before': system_state()}
    try:
        time.sleep(3)
        result['window'] = front(b.pid)
        result['memory_empty'] = sample(b.pid, b.webkit_before, 3, 2)
        sites = json.loads(args.sites.read_text())
        handles, loads = [], []
        for i, site in enumerate(sites):
            front(b.pid)
            handle, record = load_site(b, site)
            handles.append(handle)
            loads.append(record)
            if i == 0:
                b.close_initial()
            page = record['page'] if isinstance(record['page'], dict) else {}
            log(f'  {b.name}: {site["name"]}: fcp {page.get("fcp")} lcp {page.get("lcp")} load {page.get("load")} '
                f'{page.get("visibility")} {record["problem"] or ""}{" TIMEOUT" if record["timed_out"] else ""}')
        result['loads'] = loads
        result['tabs_open'] = b.tabs()
        ready = time.monotonic()
        time.sleep(max(0, ready + args.live_at - time.monotonic()))
        result['memory_live'] = sample(b.pid, b.webkit_before)
        if isinstance(b, Atrium):
            result['atrium_live'] = b.memory_api()
        log(f'  {b.name}: live {result["memory_live"]["footprint_mib"]} MiB footprint, {result["memory_live"]["processes"]} processes')
        time.sleep(max(0, ready + args.idle_at - time.monotonic()))
        result['memory_idle'] = sample(b.pid, b.webkit_before)
        if isinstance(b, Atrium):
            result['atrium_idle'] = b.memory_api()
        log(f'  {b.name}: idle {result["memory_idle"]["footprint_mib"]} MiB footprint, {result["memory_idle"]["processes"]} processes')
        front(b.pid)
        result['switch_back'] = []
        overhead = round_trip(b, handles[-1])
        result['eval_round_trip_s'] = round(overhead, 3)
        for handle, record, site in zip(handles[:3], loads[:3], sites[:3]):
            origin = record['page'].get('origin') if isinstance(record['page'], dict) else None
            seconds, state = wait_visible(b, handle, origin)
            net = round(max(0.0, seconds - overhead), 2) if seconds is not None else None
            result['switch_back'].append({'name': site['name'], 'seconds': seconds, 'seconds_minus_overhead': net, 'state': state})
            time.sleep(1)
        log(f'  {b.name}: switch back {[(s["seconds_minus_overhead"], (s["state"] or {}).get("reloaded") if isinstance(s["state"], dict) else s["state"]) for s in result["switch_back"]]}')
        result['system_after'] = system_state()
    finally:
        b.stop()
        time.sleep(5)
    return result


def summarize(runs):
    names = sorted({r[k]['browser'] for r in runs for k in r if isinstance(r[k], dict) and 'browser' in r[k]})
    sites = [s['name'] for s in json.loads(args.sites.read_text())]
    med = lambda vs: round(statistics.median(vs), 1) if vs else None
    out = {}
    for name in names:
        per = [r[k] for r in runs for k in r if isinstance(r[k], dict) and r[k].get('browser') == name and 'memory_idle' in r[k]]
        timing = {}
        for metric in ('ttfb', 'fcp', 'dcl', 'load', 'lcp'):
            per_site = {}
            for site in sites:
                values = [l['page'][metric] for p in per for l in p['loads'] if l['name'] == site and isinstance(l['page'], dict)
                          and not l['problem'] and l['page'].get(metric) is not None]
                per_site[site] = med(values)
            timing[metric] = per_site
        out[name] = {
            'runs': len(per),
            'startup_s': med([p['startup_s'] for p in per]),
            'empty_footprint_mib': med([p['memory_empty']['footprint_mib'] for p in per]),
            'live_footprint_mib': med([p['memory_live']['footprint_mib'] for p in per]),
            'live_rss_mib': med([p['memory_live']['rss_mib'] for p in per]),
            'live_processes': med([p['memory_live']['processes'] for p in per]),
            'idle_footprint_mib': med([p['memory_idle']['footprint_mib'] for p in per]),
            'idle_rss_mib': med([p['memory_idle']['rss_mib'] for p in per]),
            'idle_processes': med([p['memory_idle']['processes'] for p in per]),
            'live_footprint_runs': [p['memory_live']['footprint_mib'] for p in per],
            'idle_footprint_runs': [p['memory_idle']['footprint_mib'] for p in per],
            'timing_ms_median_per_site': timing,
            'switch_back_s_minus_overhead': {s: med([x['seconds_minus_overhead'] for p in per for x in p['switch_back']
                                                     if x['name'] == s and x['seconds_minus_overhead'] is not None]) for s in sites[:3]},
            'switch_back_reloads': sum(1 for p in per for x in p['switch_back'] if isinstance(x['state'], dict) and x['state'].get('reloaded')),
            'switch_back_total': sum(len(p['switch_back']) for p in per),
            'problems': sorted({f"{l['name']}: {l['problem']}" for p in per for l in p['loads'] if l['problem']}),
        }
    return out


def save(runs, final=False):
    report = {'date': date.today().isoformat(), 'partial': not final, 'method': __doc__.strip(),
              'window': WINDOW, 'live_at_s': args.live_at, 'idle_at_s': args.idle_at, 'runs': runs}
    if runs:
        report['summary'] = summarize(runs)
    args.output.write_text(json.dumps(report, indent=2) + '\n')
    return report


if __name__ == '__main__':
    SCRATCH.mkdir(parents=True, exist_ok=True)
    order = args.browsers.split(',')
    runs = []
    for run in range(args.runs):
        rotated = order[run % len(order):] + order[:run % len(order)]
        record = {'run': run + 1, 'order': rotated}
        for which in rotated:
            log(f'run {run + 1}: {which}; system {system_state()}')
            try:
                record[which] = run_browser(which, run)
            except Exception as error:
                log(f'run {run + 1}: {which} failed: {error!r}')
                record[which] = {'error': repr(error)[:500]}
            save(runs + [record])
        runs.append(record)
    report = save(runs, final=True)
    log('saved', args.output)
    print(json.dumps(report['summary'], indent=2))
