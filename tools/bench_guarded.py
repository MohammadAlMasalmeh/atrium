#!/usr/bin/env python3
"""Repeated, guarded memory comparison of this CEF build and the installed Aside.

Each repetition starts a fresh CEF instance (background window mode) and a fresh
headless Aside profile, one at a time, alternating which runs first. Both are
watched by window_audit (killed on any visible window, focus, or Dock icon) and
the shared watchdog. Each browser is sampled while every page is live and again
after the CEF lifecycle policy has had time to evict background pages; the
baseline has no eviction, so its second phase measures the same idle time.

Workloads:
  fixture           ten same-origin local fixture pages
  fixture-distinct  ten separate *.localhost sites
  real              public websites from real_sites.json (signed out)

The report keeps titles, URL paths, and DOM counts only (no page text, no IPs).
"""
import argparse
import json
import statistics
import time
import urllib.error
from datetime import date
from pathlib import Path
from urllib.parse import urlparse

from bench_common import (AsideBaseline, CefInstance, FixtureServer, SleepDetector, snapshot, spread, summarize_samples,
                          system_state, wait_for_quiet)

ROOT = Path(__file__).resolve().parent
parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
parser.add_argument('--workload', choices=['fixture', 'fixture-distinct', 'real'], required=True)
parser.add_argument('--runs', type=int, default=3)
parser.add_argument('--app', type=Path, default=ROOT.parent / 'target/bundle/atrium.app/Contents/MacOS/atrium')
parser.add_argument('--port', type=int, default=7871)
parser.add_argument('--fixture-port', type=int, default=7870)
parser.add_argument('--sites', type=Path, default=ROOT / 'real_sites.json')
parser.add_argument('--cef-only', action='store_true', help='skip the Aside baseline')
parser.add_argument('--max-load', type=float, default=12.0, help='wait for this 1-minute load average before each browser')
parser.add_argument('--quiet-timeout', type=int, default=600)
parser.add_argument('--output', type=Path, required=True)
args = parser.parse_args()

REAL = args.workload == 'real'
# Fixture timeline (seconds after every page loaded): CEF pages are protected for
# 30 s after use, so the live phase is sampled before that and the idle phase
# after eviction plus time for released renderers to exit.
LIVE_AT, LIVE_SAMPLES, LIVE_EVERY = (75, 8, 3) if REAL else (6, 6, 2)
IDLE_AT, IDLE_SAMPLES, IDLE_EVERY = (215, 8, 3) if REAL else (90, 6, 3)
EXPRESSION = """({url:location.href,title:document.title,ready:document.readyState,
visibility:document.visibilityState,viewport:[innerWidth,innerHeight,devicePixelRatio],
textLength:document.body?.innerText.length || 0, preview:document.body?.innerText.slice(0,350) || '',
nodes:document.querySelectorAll('*').length,resources:performance.getEntriesByType('resource').length,
iframes:document.querySelectorAll('iframe').length,userAgent:navigator.userAgent,webdriver:navigator.webdriver,
brands:(navigator.userAgentData?.brands || []).map(b => b.brand + '/' + b.version).join(', ')})"""


def log(*items, **kwargs):
    print(time.strftime('%H:%M:%S'), *items, flush=True)


def workload():
    if REAL:
        return json.loads(args.sites.read_text())
    host = lambda i: f'http://site-{i}.localhost:{args.fixture_port}' if args.workload == 'fixture-distinct' else f'http://127.0.0.1:{args.fixture_port}'
    return [{'name': f'benchmark-{i}', 'url': f'{host(i)}/page/benchmark-{i}'} for i in range(10)]


def valid(info, site):
    if not REAL:
        return None if info.get('title', '').startswith('Fixture ') and info.get('ready') == 'complete' else 'Fixture did not load'
    hosts = [urlparse(site['url']).hostname, *site.get('redirect_hosts', [])]
    if urlparse(info.get('url', '')).hostname not in hosts:
        return 'Unexpected redirect or browser error'
    if urlparse(info.get('url', '')).hostname == urlparse(site['url']).hostname and urlparse(info['url']).path.rstrip('/') != urlparse(site['url']).path.rstrip('/'):
        return 'Page navigated away from the requested URL'
    title, preview = info.get('title', '').lower(), info.get('preview', '').lower()
    if any(p in title for p in ['just a moment', 'access denied', 'security check', 'too many requests', 'error', 'prove your humanity']):
        return 'Challenge or error page'
    if any(p in preview for p in ["you've been blocked", 'verify you are human', 'unusual traffic from your computer']):
        return 'Challenge or network block'
    if info.get('ready') not in ('interactive', 'complete') or info.get('textLength', 0) < 80 or info.get('nodes', 0) < 20:
        return 'Page not ready or insufficient rendered content'
    return None


def public(info):
    # Challenge pages can contain an IP address, so page text never leaves this process.
    if not isinstance(info, dict):
        return {'error': str(info)[:200]}
    result = {k: v for k, v in info.items() if k != 'preview'}
    url = urlparse(info.get('url', ''))
    result['url'] = url.path if url.hostname in ('127.0.0.1', 'localhost') or (url.hostname or '').endswith('.localhost') \
        else url._replace(query='', fragment='', netloc=url.hostname or '').geturl()
    return result


def sample_phase(pid, count, every, live_views=None):
    samples, views = [], []
    for i in range(count):
        samples.append(snapshot(pid))
        if live_views:
            views.append(live_views())
        if i + 1 < count:
            time.sleep(every)
    return {**summarize_samples(samples), 'raw_totals': [{'footprint_mib': s['footprint_mib'], 'rss_mib': s['rss_mib'],
                                                          'process_count': s['process_count']} for s in samples if s],
            **({'cef_live_views': views} if live_views else {})}


def wait_until(start, offset):
    remaining = start + offset - time.monotonic()
    if remaining > 0:
        time.sleep(remaining)


def run_cef(run, exclude=()):
    inst = CefInstance(args.app, args.port, f'/private/tmp/atrium-perf-bench-{run}')
    result = {'browser': 'CEF build'}
    try:
        sites = workload()
        for site in sites:
            inst.command(cmd='newTab', url=site['url'], background=False)
            site['cef_id'] = inst.api('/api/browser/tabs')['tabs'][-1]['id']
        accepted, excluded = [], []
        if REAL:
            time.sleep(20)
            for site in sites:
                try:
                    inst.api('/api/browser/memory', timeout=30)
                except Exception as error:
                    raise RuntimeError(f'CEF browser stopped responding: {error}') from error
                info = inspect_cef(inst, site['cef_id'])
                reason = valid(info, site) if isinstance(info, dict) else str(info)
                if not reason and site['name'] in exclude:
                    reason = 'Excluded because it failed in the baseline'
                (excluded if reason else accepted).append({'name': site['name'], 'reason': reason, 'page': public(info)})
            rejected = {s['cef_id'] for s in sites if any(e['name'] == s['name'] for e in excluded)}
            for tab_id in rejected:
                inst.command(cmd='close', id=tab_id)
            for _ in range(60):
                if not rejected & {t['id'] for t in inst.api('/api/browser/tabs')['tabs']}:
                    break
                time.sleep(.5)
            # Renew agent protection so every accepted page is live in the first phase.
            for site in sites:
                if any(a['name'] == site['name'] for a in accepted):
                    inspect_cef(inst, site['cef_id'])
        else:
            for _ in range(80):
                tabs = inst.api('/api/browser/tabs')['tabs']
                if sum(not t['loading'] and t['title'].startswith('Fixture ') for t in tabs) == len(sites):
                    break
                time.sleep(.5)
            else:
                raise RuntimeError('CEF fixture workload did not load')
            accepted = [{'name': s['name'], 'reason': None} for s in sites]
            result['active_page'] = public(inst.evaluate(inst.api('/api/browser/tabs')['active'], 'return ' + EXPRESSION + ';'))
        start = time.monotonic()
        def views():
            try:
                return inst.api('/api/browser/memory', timeout=30)['live_renderers']
            except (AssertionError, urllib.error.URLError, TimeoutError) as error:
                log('CEF: live-view count unavailable:', str(error)[:120])
                return None
        log('CEF: pages ready; live phase')
        wait_until(start, LIVE_AT)
        result['live'] = sample_phase(inst.pid, LIVE_SAMPLES, LIVE_EVERY, views)
        wait_until(start, IDLE_AT)
        log('CEF: idle phase')
        result['idle'] = sample_phase(inst.pid, IDLE_SAMPLES, IDLE_EVERY, views)
        result['accepted'], result['excluded'] = accepted, excluded
    finally:
        result['guard'] = inst.stop()
        text = inst.log_path.read_text(errors='replace')
        result['log_lines'] = {'total': text.count('\n'), 'error': text.count(':ERROR:'), 'fatal': text.count(':FATAL:'),
                               'panic': text.count('panicked at')}
    return result


def inspect_cef(inst, tab_id):
    try:
        inst.command(cmd='activate', id=tab_id)
        last = None
        for _ in range(30):
            try:
                info = inst.evaluate(tab_id, 'return ' + EXPRESSION + ';')
                last = info
                if info.get('ready') in ('interactive', 'complete') and info.get('textLength', 0) > 80:
                    return info
            except (AssertionError, RuntimeError, urllib.error.URLError, json.JSONDecodeError) as error:
                last = str(error)
            time.sleep(1)
        return last
    except Exception as error:  # a site that breaks inspection is excluded, not fatal
        return str(error)


def run_baseline(run):
    base = AsideBaseline(f'/private/tmp/aside-perf-baseline-{run}')
    result = {'browser': 'Aside (headless)', 'version': base.version.get('Browser')}
    try:
        sites = workload()
        for site in sites:
            site['target'] = base.open(site['url'])
        base.close_other_pages({site['target'] for site in sites})
        accepted, excluded = [], []
        if REAL:
            time.sleep(20)
        deadline = time.monotonic() + (60 if REAL else 40)
        pending = list(sites)
        while pending and time.monotonic() < deadline:
            for site in list(pending):
                try:
                    site['target'] = base.resolve(site['target'], site['url'])
                    info = base.evaluate(site['target'], EXPRESSION)
                except Exception as error:
                    info = str(error)
                site['info'] = info
                ready = isinstance(info, dict) and (info.get('ready') in ('interactive', 'complete') and info.get('textLength', 0) > 80
                                                    if REAL else not valid(info, site))
                if ready:
                    pending.remove(site)
            time.sleep(.5)
        for site in sites:
            info = site.get('info', 'did not load')
            reason = valid(info, site) if isinstance(info, dict) else str(info)
            (excluded if reason else accepted).append({'name': site['name'], 'reason': reason, 'page': public(info)})
            if reason:
                base.close(site['target'])
        if not REAL and excluded:
            raise RuntimeError(f'Baseline fixture pages failed: {excluded}')
        if not REAL:
            result['active_page'] = public(sites[-1]['info'])
        start = time.monotonic()
        log('Aside: pages ready; live phase')
        wait_until(start, LIVE_AT)
        result['live'] = sample_phase(base.pid, LIVE_SAMPLES, LIVE_EVERY)
        wait_until(start, IDLE_AT)
        log('Aside: idle phase')
        result['idle'] = sample_phase(base.pid, IDLE_SAMPLES, IDLE_EVERY)
        result['accepted'], result['excluded'] = accepted, excluded
        result['pages_at_end'] = sum(t.get('type') == 'page' for t in base.json('/json/list'))
    finally:
        result['target_replacements_followed'] = base.replaced
        result['guard'] = base.stop()
    return result


def reduction(base, cand):
    return round(100 * (base - cand) / base, 1) if base and cand else None


def aggregate(runs):
    names = ['CEF build'] if args.cef_only else ['Aside (headless)', 'CEF build']
    summary = {}
    for phase in ('live', 'idle'):
        summary[phase] = {}
        for name in names:
            per = [r[name][phase] for r in runs if name in r and phase in r[name]]
            kinds = sorted({k for p in per for k in p['by_type']})
            summary[phase][name] = {
                'footprint_mib': spread([p['footprint_mib'] for p in per]),
                'rss_mib': spread([p['rss_mib'] for p in per]),
                'process_count': spread([p['process_count'] for p in per]),
                'by_type_median_of_runs': {k: {f: round(statistics.median([p['by_type'].get(k, {}).get(f, 0) or 0 for p in per]), 1)
                                               for f in ('count', 'rss_mib', 'footprint_mib')} for k in kinds},
            }
        if not args.cef_only:
            comparable = [r for r in runs if r['comparable']]
            pair = lambda field: [reduction(r['Aside (headless)'][phase][field], r['CEF build'][phase][field]) for r in comparable]
            summary[phase]['footprint_reduction_percent'] = spread(pair('footprint_mib'))
            summary[phase]['rss_reduction_percent'] = spread(pair('rss_mib'))
            summary[phase]['comparable_runs'] = len(comparable)
    return summary


fixture = None if REAL else FixtureServer(args.fixture_port)
runs = []
try:
    for run in range(args.runs):
        # Real sites run the baseline first so the CEF run can drop the same failed sites.
        order = ['cef'] if args.cef_only else ['aside', 'cef'] if REAL or run % 2 else ['cef', 'aside']
        record = {'run': run + 1, 'order': order}
        for which in order:
            quiet = wait_for_quiet(args.max_load, args.quiet_timeout, log)
            log(f'run {run + 1}: {which}; system {quiet}')
            baseline_failed = {e['name'] for e in record.get('Aside (headless)', {}).get('excluded', [])}
            sleep = SleepDetector()
            outcome = run_cef(run, baseline_failed) if which == 'cef' else run_baseline(run)
            outcome['system_before'], outcome['system_after'] = quiet, system_state()
            outcome['system_slept_seconds'] = sleep.slept_seconds()
            record[outcome['browser']] = outcome
            if not outcome['guard']['ok']:
                raise SystemExit(f'Visibility guard failed for {outcome["browser"]}: {outcome["guard"]}')
            if outcome['system_slept_seconds'] > 5:
                raise SystemExit(f'The Mac slept for {outcome["system_slept_seconds"]} s during a measurement; rerun')
            log(outcome['browser'], 'accepted', [a['name'] for a in outcome['accepted']],
                'excluded', [(e['name'], e['reason']) for e in outcome['excluded']])
            for phase in ('live', 'idle'):
                log(outcome['browser'], phase, 'footprint', outcome[phase]['footprint_mib'], 'rss', outcome[phase]['rss_mib'],
                    'processes', outcome[phase]['process_count'],
                    {k: v.get('count') for k, v in outcome[phase]['by_type'].items()}, outcome[phase].get('cef_live_views', ''))
        if args.cef_only:
            record['comparable'] = False
        else:
            names = lambda b: sorted(a['name'] for a in record[b]['accepted'])
            record['comparable'] = names('CEF build') == names('Aside (headless)')
        runs.append(record)
        record = None
        args.output.write_text(json.dumps({'partial': True, 'runs': runs}, indent=2) + '\n')
except BaseException as error:
    unfinished = [record] if globals().get('record') else []
    args.output.write_text(json.dumps({'partial': True, 'error': repr(error)[:500], 'runs': runs + unfinished}, indent=2) + '\n')
    raise
finally:
    if fixture:
        fixture.stop()

report = {
    'date': date.today().isoformat(), 'workload': args.workload,
    'metric': ('Sum over each browser process tree of macOS physical footprint (proc_pid_rusage v2 '
               'ri_phys_footprint) and RSS, MiB. RSS double-counts shared pages. Medians per phase within a run; '
               'median/min/max across runs.'),
    'method': {
        'baseline': None if args.cef_only else ('Installed Aside started with --headless, a throwaway profile, '
                                                '--force-device-scale-factor=2 and 1024x808 targets; no windows, Prohibited activation policy. '
                                                'It sends the windowed Chrome 154 user agent and navigator.webdriver is false; its '
                                                'User-Agent Client Hints brands are empty because of the override.'),
        'candidate': 'This repository\'s release bundle with ATRIUM_BACKGROUND_WINDOW=1 and ATRIUM_MOCK_KEYCHAIN=1, fresh data directory.',
        'sequencing': ('Browsers run one at a time; real sites run the baseline first and the CEF run closes sites the '
                       'baseline rejected; fixtures alternate the order per repetition.'),
        'phases': {'live': f'{LIVE_SAMPLES} samples every {LIVE_EVERY}s from {LIVE_AT}s after pages were ready',
                   'idle': f'{IDLE_SAMPLES} samples every {IDLE_EVERY}s from {IDLE_AT}s after pages were ready'},
        'cef_policy': 'max 6 live content views (plus the shell), 30 s minimum idle, agent inspection protects 180 s',
        'visibility_guard': 'window_audit.swift (50 ms polling, SIGKILL on frontmost, visible window, or Regular policy after 1 s) and watchdog.sh',
        'caveats': ['Headless Aside renders its own browser UI (sidebar and toolbar) without any window; the CEF build '
                    'includes its shell renderer. Neither is the same as a visible, interactive window.',
                    'Background CEF pages report visibilityState hidden; headless targets report visible.',
                    'The system was shared with other agents and builds; load is recorded per browser run.'],
    },
    'runs': runs, 'summary': aggregate(runs),
}
args.output.write_text(json.dumps(report, indent=2) + '\n')
log('saved', args.output)
print(json.dumps(report['summary'], indent=2))
