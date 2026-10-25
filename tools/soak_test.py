#!/usr/bin/env python3
"""Long-running stability soak of an isolated background CEF build.

Starts its own fixture server and a fresh background instance (watched by
window_audit and the watchdog), then for --minutes cycles fixture tabs (plain,
form, scroll, draft), a few real public sites, and a looping local video:
open, close, switch, navigate, type into forms, screenshot, and let the
lifecycle policy evict and wake pages. Every --sample-every seconds it records
process-tree footprint/RSS, process counts by type, live views, and tabs.

Growth is judged at checkpoints: every --checkpoint-every seconds the session
is reduced to the same fixed set of tabs and sampled after settling. The test
fails on a browser exit, renderer terminations, Rust panics, unexpected API
errors, new crash reports, or checkpoint growth beyond --max-growth-percent
(and --max-growth-mib) between the first and the last checkpoint.
"""
import argparse
import json
import random
import re
import statistics
import time
import traceback
import urllib.error
from pathlib import Path

from bench_common import CefInstance, FixtureServer, SleepDetector, snapshot, system_state

ROOT = Path(__file__).resolve().parent
parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
parser.add_argument('--minutes', type=float, default=30)
parser.add_argument('--app', type=Path, default=ROOT.parent / 'target/bundle/atrium.app/Contents/MacOS/atrium')
parser.add_argument('--port', type=int, default=7873)
parser.add_argument('--fixture-port', type=int, default=7872)
parser.add_argument('--data-dir', default='/private/tmp/atrium-perf-soak')
parser.add_argument('--real', nargs='*', default=['https://en.wikipedia.org/wiki/Web_browser',
                                                  'https://developer.mozilla.org/en-US/docs/Web/API',
                                                  'https://news.ycombinator.com/'])
parser.add_argument('--target-tabs', type=int, default=12, help='open tabs the action mix hovers around')
parser.add_argument('--action-every', type=float, default=4)
parser.add_argument('--sample-every', type=float, default=30)
parser.add_argument('--checkpoint-every', type=float, default=300)
parser.add_argument('--max-growth-percent', type=float, default=25)
parser.add_argument('--max-growth-mib', type=float, default=250)
parser.add_argument('--max-process-growth', type=float, default=4)
parser.add_argument('--seed', type=int, default=7)
parser.add_argument('--output', type=Path, required=True)
args = parser.parse_args()
rng = random.Random(args.seed)
sleep_detector = SleepDetector()
started = time.monotonic()
elapsed = lambda: round(time.monotonic() - started, 1)


def log(*items):
    print(time.strftime('%H:%M:%S'), f'[{elapsed():7.1f}s]', *items, flush=True)


fixture = FixtureServer(args.fixture_port)
F = fixture.url
crash_dir = Path.home() / 'Library/Logs/DiagnosticReports'
reports_before = {p.name for p in crash_dir.glob('*')} if crash_dir.exists() else set()
try:
    inst = CefInstance(args.app, args.port, args.data_dir)
except Exception:
    fixture.stop()
    raise
counters = {k: 0 for k in ['opened', 'closed', 'switched', 'woken', 'navigated', 'typed', 'screenshots',
                           'video_checks', 'video_playing', 'evictions_seen', 'api_errors', 'expected_errors']}
api_errors, samples, checkpoints = [], [], []
fixed = {}  # name -> tab id kept for the whole session
serial = 0


def call(label, fn, expected=()):
    """Run one API action; unexpected failures are recorded, expected ones counted."""
    try:
        return fn()
    except Exception as error:
        text = f'{type(error).__name__}: {error}'
        if any(e in text for e in expected):
            counters['expected_errors'] += 1
        else:
            counters['api_errors'] += 1
            api_errors.append({'t': elapsed(), 'action': label, 'error': text[:300]})
            log('API error', label, text[:200])
        return None


def tabs():
    return inst.api('/api/browser/tabs')


def open_tab(url, background=True):
    inst.command(cmd='newTab', url=url, background=background)
    counters['opened'] += 1
    return tabs()['tabs'][-1]['id']


def fixture_url():
    global serial
    serial += 1
    kind = rng.choice(['page', 'page', 'form', 'scroll', 'draft'])
    host = F if rng.random() < .5 else F.replace('127.0.0.1', f'site-{serial % 7}.localhost')
    return f'{host}/{kind}/soak-{serial}'


def churn_ids(state):
    keep = set(fixed.values())
    return [t['id'] for t in state['tabs'] if t['id'] not in keep and t['kind'] == 'web']


def act():
    state = tabs()
    churn = churn_ids(state)
    choices = ['open'] * (3 if len(state['tabs']) < args.target_tabs else 1) + ['close'] * (3 if len(state['tabs']) > args.target_tabs else 1)
    choices += ['switch'] * 3 + ['type'] * 2 + ['navigate', 'screenshot', 'real', 'video']
    action = rng.choice(choices)
    if action == 'open':
        call('open', lambda: open_tab(fixture_url(), background=rng.random() < .6))
    elif action == 'close' and churn:
        victim = rng.choice(churn)
        call('close', lambda: inst.command(cmd='close', id=victim))
        counters['closed'] += 1
    elif action == 'switch' and state['tabs']:
        target = rng.choice(state['tabs'])
        if target['suspended']:
            counters['woken'] += 1
        call('switch', lambda: inst.command(cmd='activate', id=target['id']))
        counters['switched'] += 1
    elif action == 'type':
        forms = [t for t in state['tabs'] if '/form/' in t['url']]
        if not forms:
            call('open', lambda: open_tab(f'{F}/form/soak-typing-{serial}', background=True))
            return
        target = rng.choice(forms)

        def type_into():
            inst.focus(target['id'])
            tree = '\n'.join(c.get('text', '') for c in inst.client.tool('snapshot', {'interactive': True}))
            refs = re.findall(r'textbox[^\n]*?\b(e\d+)\b', tree)
            if not refs:
                raise AssertionError('no text fields in snapshot: ' + tree[:200])
            inst.client.tool('type', {'ref': rng.choice(refs), 'text': f'soak text {serial} ' * rng.randint(1, 4)})
            counters['typed'] += 1
        call('type', type_into, expected=('still loading',))
    elif action == 'navigate' and churn:
        target = rng.choice(churn)

        def navigate():
            inst.focus(target)
            inst.client.tool('navigate', {'url': fixture_url()})
            counters['navigated'] += 1
        call('navigate', navigate, expected=('still loading',))
    elif action == 'screenshot':
        def shot():
            images = [c for c in inst.client.tool('screenshot', {}) if c.get('type') == 'image']
            assert images and images[0].get('data'), 'empty screenshot'
            counters['screenshots'] += 1
        call('screenshot', shot, expected=('not ready to capture', 'no page', 'has no page'))
    elif action == 'real' and args.real:
        name = rng.choice([k for k in fixed if k.startswith('real')] or [None])
        if name:
            call('switch real', lambda: inst.command(cmd='activate', id=fixed[name]))
            counters['switched'] += 1
    elif action == 'video' and 'video' in fixed:
        def check():
            inst.command(cmd='activate', id=fixed['video'])
            time.sleep(1.5)
            first = inst.evaluate(fixed['video'], "const v=document.querySelector('#video'); return {t:v.currentTime, paused:v.paused};")
            time.sleep(1.5)
            second = inst.evaluate(fixed['video'], "const v=document.querySelector('#video'); return {t:v.currentTime, paused:v.paused};")
            counters['video_checks'] += 1
            if second['t'] != first['t'] and not second['paused']:
                counters['video_playing'] += 1
        call('video', check, expected=('still loading',))


def record_sample(kind='periodic'):
    snap = snapshot(inst.pid)
    memory = inst.api('/api/browser/memory')
    entry = {'t': elapsed(), 'kind': kind, 'tabs': len(memory['tabs']), 'live_views': memory['live_renderers'],
             'suspended': sum(t['suspended'] for t in memory['tabs']), **(snap or {}), 'system': system_state()}
    samples.append(entry)
    return entry


def checkpoint():
    """Reduce to the fixed tabs, let eviction settle, and sample the same state each time."""
    state = tabs()
    for tab_id in churn_ids(state):
        call('checkpoint close', lambda tab_id=tab_id: inst.command(cmd='close', id=tab_id))
        counters['closed'] += 1
    # Wake every fixed tab so each checkpoint measures the same live set.
    for name in [n for n in fixed if n != 'anchor'] + ['anchor']:
        call('checkpoint activate', lambda name=name: inst.command(cmd='activate', id=fixed[name]))
        time.sleep(2)
    time.sleep(20)
    values = []
    for _ in range(5):
        values.append(record_sample('checkpoint'))
        time.sleep(3)
    point = {'t': elapsed(), 'tabs': values[-1]['tabs'], 'live_views': values[-1]['live_views'],
             'footprint_mib': statistics.median([v['footprint_mib'] for v in values if v.get('footprint_mib') is not None] or [0]),
             'rss_mib': statistics.median([v['rss_mib'] for v in values]),
             'process_count': statistics.median([v['process_count'] for v in values]),
             'browser_footprint_mib': statistics.median([v['by_type']['browser']['footprint_mib'] for v in values])}
    checkpoints.append(point)
    log('checkpoint', json.dumps(point))


failure = None
try:
    fixed['anchor'] = open_tab(f'{F}/page/soak-anchor', background=False)
    fixed['video'] = open_tab(f'{F}/video/soak-video', background=True)
    for i, url in enumerate(args.real):
        fixed[f'real-{i}'] = open_tab(url, background=True)
    for _ in range(6):
        open_tab(fixture_url(), background=True)
    time.sleep(20)
    checkpoint()
    next_sample = next_checkpoint = time.monotonic()
    next_checkpoint += args.checkpoint_every
    previous_suspended = set()
    deadline = started + args.minutes * 60
    while time.monotonic() < deadline:
        if inst.process.poll() is not None:
            raise AssertionError(f'browser exited with code {inst.process.returncode}')
        act()
        suspended = {t['id'] for t in inst.api('/api/browser/memory')['tabs'] if t['suspended']}
        counters['evictions_seen'] += len(suspended - previous_suspended)
        previous_suspended = suspended
        if time.monotonic() >= next_sample:
            entry = record_sample()
            log('sample', f"tabs {entry['tabs']} live {entry['live_views']} processes {entry['process_count']} "
                f"footprint {entry['footprint_mib']} rss {entry['rss_mib']}", json.dumps(counters))
            next_sample = time.monotonic() + args.sample_every
        if time.monotonic() >= next_checkpoint and deadline - time.monotonic() > 60:
            checkpoint()
            next_checkpoint = time.monotonic() + args.checkpoint_every
        time.sleep(args.action_every * rng.uniform(.5, 1.5))
    checkpoint()
except Exception as error:
    failure = f'{type(error).__name__}: {error}'
    log('FAILED', failure)
    traceback.print_exc()
finally:
    alive_at_end = inst.process.poll() is None
    guard = inst.stop()
    fixture.stop()

text = inst.log_path.read_text(errors='replace')
terminations = [line for line in text.splitlines() if 'renderer terminated' in line]
panics = [line for line in text.splitlines() if 'panicked at' in line]
chromium_line = re.compile(r'^\[\d+:\d+:')
rust_errors = [line for line in text.splitlines() if not chromium_line.match(line)
               and re.search(r'error|could not|failed|panicked', line, re.I) and 'opencode' not in line.lower()]
chromium_errors = [line for line in text.splitlines() if ':ERROR:' in line or ':FATAL:' in line]
child_crashes = [line for line in chromium_errors if re.search(r'exited unexpectedly|crashed|:FATAL:', line)]
new_reports = sorted(p.name for p in crash_dir.glob('*') if p.name not in reports_before and 'atrium' in p.name.lower()) if crash_dir.exists() else []
first, last = (checkpoints[0], checkpoints[-1]) if checkpoints else (None, None)
growth = None
if first and last:
    delta = last['footprint_mib'] - first['footprint_mib']
    growth = {'footprint_mib': round(delta, 1), 'footprint_percent': round(100 * delta / first['footprint_mib'], 1),
              'browser_process_mib': round(last['browser_footprint_mib'] - first['browser_footprint_mib'], 1),
              'process_count': last['process_count'] - first['process_count']}
problems = []
if failure:
    problems.append(failure)
if not alive_at_end:
    problems.append('browser was not running at the end')
if not guard['ok']:
    problems.append('visibility guard violation')
if terminations:
    problems.append(f'{len(terminations)} renderer terminations')
if child_crashes:
    problems.append(f'{len(child_crashes)} Chromium child-process crashes')
if panics:
    problems.append(f'{len(panics)} Rust panics')
if counters['api_errors']:
    problems.append(f"{counters['api_errors']} unexpected API errors")
if new_reports:
    problems.append(f'{len(new_reports)} new crash reports')
if growth and growth['footprint_percent'] > args.max_growth_percent and growth['footprint_mib'] > args.max_growth_mib:
    problems.append(f"checkpoint footprint grew {growth['footprint_mib']} MiB ({growth['footprint_percent']}%)")
if growth and growth['process_count'] > args.max_process_growth:
    problems.append(f"checkpoint process count grew by {growth['process_count']}")
if len(checkpoints) < 3:
    problems.append('fewer than three checkpoints; run longer')
slept = sleep_detector.slept_seconds()
if slept > 5:
    problems.append(f'the Mac slept for {slept} s during the soak; the result does not represent continuous use')
report = {'minutes': args.minutes, 'seed': args.seed, 'passed': not problems, 'problems': problems, 'system_slept_seconds': slept,
          'counters': counters, 'checkpoints': checkpoints, 'growth_first_to_last_checkpoint': growth,
          'renderer_terminations': terminations[:20], 'child_process_crashes': child_crashes[:20],
          'rust_panics': panics[:20], 'rust_error_lines': {'count': len(rust_errors), 'first': rust_errors[:20]},
          'chromium_error_lines': {'count': len(chromium_errors), 'first': [l[-200:] for l in chromium_errors[:10]]},
          'new_crash_reports': new_reports, 'api_errors': api_errors[:50], 'visibility_guard': guard,
          'samples': samples, 'real_sites': [u.split('/')[2] for u in args.real]}
args.output.write_text(json.dumps(report, indent=2) + '\n')
log('PASS' if not problems else 'FAIL', problems, 'report', args.output)
raise SystemExit(0 if not problems else 1)
