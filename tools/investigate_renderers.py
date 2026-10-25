#!/usr/bin/env python3
"""Attribute renderer processes to tabs, evicted views, cross-site subframes, and history.

Starts its own background instance with a fresh profile, drives it through the shell
command API (the agent would protect pages from eviction), and counts the instance's
child processes by Chromium process type after each phase. Fixture sites are
site-N.localhost hosts on the fixture server; Chromium resolves *.localhost to
loopback and treats each as a separate site.
"""
import argparse
import collections
import json
import subprocess
import tempfile
import time
import urllib.error
from pathlib import Path
import atrium_api
from memory_benchmark import physical_footprint_mib

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--app', type=Path, default=Path('target/bundle/atrium.app/Contents/MacOS/atrium'))
parser.add_argument('--port', type=int, default=7794)
parser.add_argument('--fixture-port', type=int, default=7789)
parser.add_argument('--real-sites', type=Path, help='also open these sites (real_sites.json format) and count their subframes')
parser.add_argument('--per-site', action='store_true', help='open each real site alone, list its iframes, then close it')
parser.add_argument('--skip-fixtures', action='store_true')
parser.add_argument('--output', type=Path)
args = parser.parse_args()
report = []


def site(name, path='/page/x'):
    return f'http://{name}.localhost:{args.fixture_port}{path}'


def processes(root):
    rows = subprocess.check_output(['ps', '-axo', 'pid=,ppid=,rss=,args='], text=True)
    table = {}
    for row in rows.splitlines():
        pid, parent, rss, command = row.strip().split(None, 3)
        table[int(pid)] = (int(parent), int(rss), command)
    tree, frontier = set(), {root}
    while frontier:
        tree |= frontier
        frontier = {pid for pid, (parent, _, _) in table.items() if parent in frontier} - tree
    return {pid: table[pid] for pid in tree if pid in table}


def kind(command):
    for flag in ('--utility-sub-type=', '--type='):
        if flag in command:
            return command.split(flag, 1)[1].split()[0].rsplit('.', 1)[-1]
    return 'browser'


def sample(label, root, settle=4):
    time.sleep(settle)
    tree = processes(root)
    kinds = collections.Counter(kind(c) for _, _, c in tree.values())
    renderers = [rss for _, rss, c in tree.values() if kind(c) == 'renderer']
    memory = api('/api/browser/memory')
    entry = {
        'phase': label,
        'tabs': len(memory['tabs']),
        'live_views': memory['live_renderers'],
        'processes': len(tree),
        'renderers': len(renderers),
        'kinds': dict(kinds),
        'renderer_rss_mib': round(sum(renderers) / 1024, 1),
        'footprint_mib': physical_footprint_mib(list(tree)),
    }
    report.append(entry)
    print(f"{label:58} tabs {entry['tabs']:2} views {entry['live_views']:2} renderers {entry['renderers']:2} "
          f"processes {entry['processes']:2} renderer RSS {entry['renderer_rss_mib']} MiB", flush=True)
    return entry


def tabs():
    return api('/api/browser/tabs')['tabs']


def open_tabs(urls, background=True):
    before = {t['id'] for t in tabs()}
    for url in urls:
        command(cmd='newTab', url=url, background=background)
    for _ in range(300):
        new = [t for t in tabs() if t['id'] not in before]
        if len(new) == len(urls) and not any(t['loading'] for t in new):
            return [t['id'] for t in new]
        time.sleep(.2)
    raise AssertionError(f'Tabs did not load: {new}')


def close(ids):
    for id in ids:
        command(cmd='close', id=id)
    for _ in range(100):
        if not {t['id'] for t in tabs()} & set(ids):
            return
        time.sleep(.2)
    raise AssertionError('Tabs did not close')


def navigate_active(url):
    command(cmd='navigate', url=url)
    for _ in range(150):
        active = next(t for t in tabs() if t['id'] == api('/api/browser/tabs')['active'])
        if active['url'] == url and not active['loading']:
            return
        time.sleep(.2)
    raise AssertionError(f'Navigation to {url} did not finish')


def fixtures(root):
    ids = open_tabs([site(f'site-{i}') for i in range(6)])
    sample('6 tabs on 6 sites', root)
    close(ids)
    sample('closed them', root)
    sample('closed them, 20 s later', root, settle=20)

    ids = open_tabs([site(f'site-{i}') for i in range(10, 20)])
    sample('10 tabs on 10 sites', root)
    for _ in range(60):
        if sum(t['suspended'] for t in tabs() if t['id'] in ids) >= 4:
            break
        time.sleep(2)
    sample('after eviction to the six-view budget', root)
    close(ids)
    sample('closed them', root, settle=10)

    ids = open_tabs([site(f'same-{i}', '/page/same') for i in range(1)] * 4)
    sample('4 tabs on one site', root)
    close(ids)

    frames = 'frame-1.localhost,frame-2.localhost,frame-3.localhost'
    ids = open_tabs([site('host-1', f'/frames/{frames}')])
    sample('1 tab with 3 cross-site iframes', root)
    ids += open_tabs([site(f'host-{i}', f'/frames/{frames}') for i in (2, 3)])
    sample('3 tabs on 3 sites, each with the same 3 iframe sites', root)
    close(ids[:1])
    sample('closed one of them', root)
    close(ids[1:])
    sample('closed the rest', root, settle=10)

    open_tabs([site('nav-0')], background=False)
    for i in range(1, 6):
        navigate_active(site(f'nav-{i}'))
    sample('1 tab navigated across 6 sites', root)
    sample('same, 30 s later', root, settle=30)


with tempfile.TemporaryDirectory(prefix='atrium-renderers-', dir='/private/tmp') as directory:
    profile = Path(directory)
    client = atrium_api.Client(f'http://127.0.0.1:{args.port}', data_dir=profile)
    api, command = client.api, client.command
    log = (profile / 'launch.log').open('w')
    process = subprocess.Popen([str(args.app.resolve()), 'run', '--data-dir', str(profile), '--port', str(args.port),
                                '--opencode-url', 'http://127.0.0.1:1'], stdout=log, stderr=log, env=atrium_api.instance_env())
    try:
        for _ in range(240):
            try:
                if api('/api/browser/memory').get('shell_ready'):
                    break
            except (urllib.error.URLError, TimeoutError):
                pass
            time.sleep(.5)
        root = process.pid
        sample('baseline (shell and startup tab)', root)

        if not args.skip_fixtures:
            fixtures(root)

        if args.real_sites and args.per_site:
            for entry in json.loads(args.real_sites.read_text()):
                try:
                    [id] = open_tabs([entry['url']], background=False)
                except AssertionError as error:
                    print('skipped', entry['name'], error, flush=True)
                    continue
                opened = sample(f"{entry['name']} alone", root, settle=10)
                listed = [t for t in tabs() if t['kind'] != 'scratch']
                client.tool('switch_tab', {'index': next(i for i, t in enumerate(listed) if t['id'] == id)})
                frames = client.tool('evaluate', {'code': """
                    const origins = [...document.querySelectorAll('iframe')].map(f => { try { return new URL(f.src).origin; } catch { return f.src ? 'invalid' : '(no src)'; } });
                    const workers = navigator.serviceWorker ? (await navigator.serviceWorker.getRegistrations()).map(r => r.scope) : [];
                    return { url: location.href, top_iframes: origins.length, iframe_origins: [...new Set(origins)], service_workers: workers };
                """})
                opened['frames'] = json.loads(frames[0]['text'])
                print('   ', json.dumps(opened['frames']), flush=True)
                close([id])
                sample(f"{entry['name']} closed", root)
                sample(f"{entry['name']} closed, 35 s later", root, settle=35)
        elif args.real_sites:
            # Foreground, like a user looking at each page, so lazy embeds load too.
            ids = []
            for entry in json.loads(args.real_sites.read_text()):
                try:
                    ids += open_tabs([entry['url']], background=False)
                    sample(f"opened {entry['name']}", root, settle=10)
                except AssertionError as error:
                    print('skipped', entry['name'], error, flush=True)
            close(ids[:2])
            sample('closed the two oldest real-site tabs', root)
            sample('same, 30 s later', root, settle=30)
            close(ids[2:])
            sample('closed all real-site tabs', root, settle=15)
            sample('same, 45 s later', root, settle=45)
    finally:
        try:
            command(cmd='quit')
            process.wait(timeout=60)
        except Exception:
            process.terminate()
        if args.output:
            args.output.write_text(json.dumps(report, indent=2))
