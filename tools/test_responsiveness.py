#!/usr/bin/env python3
"""Tab-switch latency and scroll frame timing on local fixture pages.

Starts its own fixture server and a fresh background CEF instance (watched by
window_audit and the watchdog). Uses the token-protected /api/browser/devtools
route (Manager.cdp) for Performance.getMetrics and Input.synthesizeScrollGesture.

Measured:
  live switch   activate a live tab: API round trip, the page's visibilitychange,
                and its first animation frame after the switch
  wake switch   activate an evicted tab: time until the view is live and loaded
  scroll        a synthesized 4000 px wheel-style scroll on a long page: frame
                intervals from requestAnimationFrame, plus Performance.getMetrics
                deltas (layout, style, script, task time)
Fails if an operation errors, or medians exceed --max-switch-ms/--max-wake-ms.
"""
import argparse
import json
import statistics
import time
from pathlib import Path

from bench_common import CefInstance, FixtureServer, system_state

ROOT = Path(__file__).resolve().parent
parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
parser.add_argument('--app', type=Path, default=ROOT.parent / 'target/bundle/atrium.app/Contents/MacOS/atrium')
parser.add_argument('--port', type=int, default=7875)
parser.add_argument('--fixture-port', type=int, default=7874)
parser.add_argument('--switches', type=int, default=30)
parser.add_argument('--wakes', type=int, default=5)
parser.add_argument('--scrolls', type=int, default=5)
parser.add_argument('--max-switch-ms', type=float, default=250)
parser.add_argument('--max-wake-ms', type=float, default=3000)
parser.add_argument('--output', type=Path)
args = parser.parse_args()


def log(*items):
    print(time.strftime('%H:%M:%S'), *items, flush=True)


def stats(values):
    values = sorted(v for v in values if v is not None)
    if not values:
        return None
    p = lambda q: values[min(len(values) - 1, int(round(q * (len(values) - 1))))]
    return {'n': len(values), 'median': round(statistics.median(values), 1), 'p95': round(p(.95), 1),
            'min': round(values[0], 1), 'max': round(values[-1], 1)}


RECORDER = """
const generation = window.__perfGeneration = (window.__perfGeneration || 0) + 1;
window.__perf = {visible: null, frames: [], since: performance.timeOrigin + performance.now()};
const record = () => { if (window.__perfGeneration !== generation) return;
  window.__perf.frames.push(performance.timeOrigin + performance.now());
  if (window.__perf.frames.length < 4000) requestAnimationFrame(record); };
requestAnimationFrame(record);
if (!window.__perfVis) { window.__perfVis = true;
  addEventListener('visibilitychange', () => { if (document.visibilityState === 'visible' && window.__perf.visible === null)
    window.__perf.visible = performance.timeOrigin + performance.now(); }); }
return {visibility: document.visibilityState, epoch: performance.timeOrigin + performance.now()};
"""
READ = "return {...window.__perf, visibility: document.visibilityState, scrollY, height: document.documentElement.scrollHeight};"

fixture = FixtureServer(args.fixture_port)
inst = None
problems, report = [], {'system_before': system_state()}
try:
    inst = CefInstance(args.app, args.port, '/private/tmp/atrium-perf-responsiveness')
    def tabs():
        return inst.api('/api/browser/tabs')

    evaluate = inst.evaluate

    ids = []
    for i in range(5):
        inst.command(cmd='newTab', url=f'{fixture.url}/scroll/responsive-{i}', background=False)
        ids.append(tabs()['tabs'][-1]['id'])
    for _ in range(80):
        if all(not t['loading'] for t in tabs()['tabs'] if t['id'] in ids):
            break
        time.sleep(.25)
    time.sleep(2)

    probe = evaluate(ids[-1], RECORDER)
    time.sleep(1)
    frames = evaluate(ids[-1], READ)
    report['active_page'] = {'visibility': probe['visibility'], 'animation_frames_in_1s': len(frames['frames'])}
    log('active page visibility', probe['visibility'], 'rAF frames in 1 s:', len(frames['frames']))

    # Live tab switches.
    switch = {'api_ms': [], 'visible_ms': [], 'first_frame_ms': []}
    details = []
    for n in range(args.switches):
        target = ids[n % len(ids)] if ids[n % len(ids)] != tabs()['active'] else ids[(n + 1) % len(ids)]
        try:
            before = evaluate(target, RECORDER)
            t0 = time.time() * 1000
            inst.command(cmd='activate', id=target)
            switch['api_ms'].append(time.time() * 1000 - t0)
            time.sleep(.4)
            seen = evaluate(target, READ)
        except Exception as error:
            details.append({'error': f'{type(error).__name__}: {error}'[:300], 'system': system_state()})
            problems.append(f'switch {n}: {details[-1]["error"]}')
            continue
        if seen['visible']:
            switch['visible_ms'].append(seen['visible'] - t0)
        after = [f for f in seen['frames'] if f >= t0]
        if after:
            switch['first_frame_ms'].append(after[0] - t0)
        details.append({'before': before['visibility'], 'after': seen['visibility'], 'frames_before_switch': len(seen['frames']) - len(after),
                        'api_ms': round(switch['api_ms'][-1], 1),
                        'first_frame_ms': round(after[0] - t0, 1) if after else None})
    report['live_switch'] = {k: stats(v) for k, v in switch.items()}
    report['live_switch']['switches'] = args.switches
    report['live_switch']['details'] = details
    log('live switch', json.dumps(report['live_switch']))

    # Scroll frame timing on the active long page.
    active = tabs()['active']
    inst.devtools('Performance.enable', tab=active)
    scrolls = []
    for n in range(args.scrolls):
        evaluate(active, 'scrollTo(0, 0); return 0;')
        time.sleep(.3)
        evaluate(active, RECORDER)
        before = {m['name']: m['value'] for m in inst.devtools('Performance.getMetrics', tab=active)['metrics']}
        t0 = time.time() * 1000
        try:
            inst.devtools('Input.synthesizeScrollGesture', {'x': 400, 'y': 400, 'yDistance': -4000, 'speed': 2000,
                                                            'gestureSourceType': 'mouse'}, tab=active)
            gesture_ok = True
        except RuntimeError as error:
            gesture_ok = str(error)
        wall = time.time() * 1000 - t0
        after = {m['name']: m['value'] for m in inst.devtools('Performance.getMetrics', tab=active)['metrics']}
        seen = evaluate(active, READ)
        frames = [f for f in seen['frames'] if t0 <= f <= t0 + wall]
        gaps = [b - a for a, b in zip(frames, frames[1:])]
        scrolls.append({'gesture': gesture_ok, 'wall_ms': round(wall, 1), 'scrolled_px': seen['scrollY'], 'frames': len(frames),
                        'frame_interval_ms': stats(gaps), 'frames_over_33ms': sum(g > 33.4 for g in gaps),
                        **{f'{k}_ms': round(1000 * (after[k] - before[k]), 1) for k in
                           ('LayoutDuration', 'RecalcStyleDuration', 'ScriptDuration', 'TaskDuration') if k in after}})
        log('scroll', json.dumps(scrolls[-1]))
    inst.devtools('Performance.disable', tab=active)
    all_gaps = [g for s in scrolls for g in [s['frame_interval_ms']] if g]
    report['scroll'] = {'runs': scrolls, 'median_of_median_frame_interval_ms':
                        round(statistics.median([g['median'] for g in all_gaps]), 1) if all_gaps else None}

    # Wake switches: grow past the live-view budget, wait for eviction, then select sleeping tabs.
    for i in range(6):
        inst.command(cmd='newTab', url=f'{fixture.url}/page/responsive-extra-{i}', background=True)
    wakes = []
    deadline = time.monotonic() + 60 + args.wakes * 60
    while len(wakes) < args.wakes and time.monotonic() < deadline:
        sleeping = [t for t in tabs()['tabs'] if t['suspended']]
        if not sleeping:
            time.sleep(2)
            continue
        target = sleeping[0]['id']
        t0 = time.monotonic()
        inst.command(cmd='activate', id=target)
        live = None
        while time.monotonic() - t0 < 15:
            tab = next(t for t in tabs()['tabs'] if t['id'] == target)
            if live is None and not tab['suspended']:
                live = (time.monotonic() - t0) * 1000
            if not tab['suspended'] and not tab['loading'] and tab['title'].startswith('Fixture '):
                wakes.append({'view_ms': round(live, 1), 'loaded_ms': round((time.monotonic() - t0) * 1000, 1)})
                break
            time.sleep(.02)
        else:
            problems.append(f'tab {target} did not wake within 15 s')
            break
        log('wake', wakes[-1])
    report['wake_switch'] = {'view_ms': stats([w['view_ms'] for w in wakes]), 'loaded_ms': stats([w['loaded_ms'] for w in wakes]),
                             'runs': wakes}
except Exception as error:
    problems.append(f'{type(error).__name__}: {error}')
    log('FAILED', problems[-1])
finally:
    report['visibility_guard'] = inst.stop() if inst else {'ok': False, 'error': 'instance did not start'}
    fixture.stop()
    report['system_after'] = system_state()

api = (report.get('live_switch') or {}).get('api_ms')
if api and api['median'] > args.max_switch_ms:
    problems.append(f"median live switch {api['median']} ms > {args.max_switch_ms}")
wake = (report.get('wake_switch') or {}).get('loaded_ms')
if wake and wake['median'] > args.max_wake_ms:
    problems.append(f"median wake {wake['median']} ms > {args.max_wake_ms}")
if len((report.get('wake_switch') or {}).get('runs', [])) < args.wakes:
    problems.append('not enough wake measurements')
if not report['visibility_guard']['ok']:
    problems.append('visibility guard violation')
report['problems'], report['passed'] = problems, not problems
if args.output:
    args.output.write_text(json.dumps(report, indent=2) + '\n')
print(json.dumps(report, indent=2))
raise SystemExit(0 if not problems else 1)
