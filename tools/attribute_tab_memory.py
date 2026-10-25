#!/usr/bin/env python3
"""Per-site memory attribution: what one more tab costs in Chrome and in this build.

For each browser (fresh profile, visible frontmost window), each site is opened in a
new foreground tab; after it settles the script records how many processes and how
much physical footprint the tab added, the page's iframes by origin, and then
closes the tab and records what was released. This separates "the page itself is
heavier" from "processes outlive their tab".
"""
import json
import time
from pathlib import Path

import bench_three_way as b3  # reuses the launchers; its argument parser reads this script's arguments

FRAMES = """(() => {
  const origins = {};
  for (const f of document.querySelectorAll('iframe')) {
    let o; try { o = new URL(f.src, location.href).origin; } catch (e) { o = 'invalid'; }
    if (!f.src || f.src === 'about:blank') o = 'about:blank';
    origins[o] = (origins[o] || 0) + 1;
  }
  return {iframes: document.querySelectorAll('iframe').length, origins,
          heap_mib: performance.memory ? Math.round(performance.memory.usedJSHeapSize / 1048576) : null};
})()"""


def main(out_path, settle):
    b3.SCRATCH.mkdir(parents=True, exist_ok=True)
    sites = json.loads(b3.args.sites.read_text())
    report = {}
    for which in b3.args.browsers.split(','):
        if which == 'chrome':
            b = b3.Chromium('Google Chrome', b3.CHROME, b3.SCRATCH / 'attr-chrome')
        elif which == 'aside':
            b = b3.Chromium('Aside', b3.ASIDE, b3.SCRATCH / 'attr-aside')
        else:
            b = b3.Atrium(b3.args.app, b3.args.port, b3.SCRATCH / 'attr-atrium')
        rows = []
        try:
            time.sleep(3)
            first = True
            for site in sites:
                try:
                    b3.front(b.pid, 1)
                except RuntimeError as error:
                    print('warning: not frontmost:', error, flush=True)
                before = b3.memory(b.pid, b.webkit_before)
                handle, record = b3.load_site(b, site)
                # Chrome closes its window with its last tab, so Chromium browsers keep their first tab.
                if first and isinstance(b, b3.Atrium):
                    b.close_initial()
                first = False
                time.sleep(settle)
                with_tab = b3.memory(b.pid, b.webkit_before)
                try:
                    frames = b.evaluate(handle, FRAMES)
                except Exception as error:
                    frames = {'error': str(error)[:120]}
                # Close it and see what comes back.
                if isinstance(b, b3.Atrium):
                    b.client.command(cmd='close', id=handle)
                else:
                    b.browser('Target.closeTarget', {'targetId': handle})
                time.sleep(settle)
                after = b3.memory(b.pid, b.webkit_before)
                diff = lambda a, z, k: round(z[k] - a[k], 1)
                row = {'name': site['name'], 'problem': record['problem'],
                       'tab_footprint_mib': diff(before, with_tab, 'footprint_mib'),
                       'tab_processes': diff(before, with_tab, 'processes'),
                       'renderers_with_tab': with_tab['by_type'].get('renderer', {}).get('count', 0),
                       'browser_process_mib': with_tab['by_type'].get('browser', {}).get('footprint_mib'),
                       'released_mib': diff(after, with_tab, 'footprint_mib'),
                       'processes_left_after_close': diff(before, after, 'processes'),
                       'frames': frames}
                rows.append(row)
                print(b.name, json.dumps({k: v for k, v in row.items() if k != 'frames'}), 'iframes',
                      frames.get('iframes'), len(frames.get('origins', {})), 'origins', flush=True)
            report[b.name] = {'rows': rows, 'final': b3.memory(b.pid, b.webkit_before)}
        finally:
            b.stop()
            time.sleep(5)
        out_path.write_text(json.dumps(report, indent=2) + '\n')


if __name__ == '__main__':
    main(Path(b3.args.output), 8)
