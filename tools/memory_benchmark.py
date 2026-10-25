#!/usr/bin/env python3
"""Sample whole browser process trees using the same RSS metric on macOS/Linux.

Example: --browser 'Baseline=/path/to/reference/browser/'
         --browser 'This build=/absolute/path/target/bundle/atrium.app/Contents/'
Use equivalent fresh profiles, pages, extensions, and warm-up time. Aggregate RSS
includes shared pages more than once; it is not macOS physical memory footprint.
"""
import argparse
import json
import statistics
import subprocess
import time
import ctypes
import sys
from pathlib import Path


class MacUsageV2(ctypes.Structure):
    # macOS SDK sys/resource.h, rusage_info_v2. Sizes are bytes.
    _fields_ = [('uuid', ctypes.c_uint8 * 16)] + [(name, ctypes.c_uint64) for name in (
        'user_time system_time pkg_idle_wkups interrupt_wkups pageins wired_size '
        'resident_size phys_footprint proc_start_abstime proc_exit_abstime '
        'child_user_time child_system_time child_pkg_idle_wkups child_interrupt_wkups '
        'child_pageins child_elapsed_abstime diskio_bytesread diskio_byteswritten').split()]


def physical_footprint_mib(pids):
    """Sum macOS per-process physical footprint; return None for incomplete coverage."""
    if sys.platform != 'darwin':
        return None
    lib = ctypes.CDLL('/usr/lib/libproc.dylib', use_errno=True)
    lib.proc_pid_rusage.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
    lib.proc_pid_rusage.restype = ctypes.c_int
    total = 0
    for pid in pids:
        usage = MacUsageV2()
        if lib.proc_pid_rusage(pid, 2, ctypes.byref(usage)) != 0:
            return None
        total += usage.phys_footprint
    return round(total / 1048576, 2)


def process_table():
    rows = subprocess.check_output(['ps', '-axo', 'pid=,ppid=,rss=,comm='], text=True)
    processes = {}
    for row in rows.splitlines():
        parts = row.strip().split(None, 3)
        if len(parts) == 4:
            pid, parent, rss, command = parts
            processes[int(pid)] = (int(parent), int(rss), command)
    return processes


def tree(processes, prefix):
    selected = {int(prefix[4:])} if prefix.startswith('pid:') else {pid for pid, (_, _, command) in processes.items() if command.startswith(prefix)}
    selected &= processes.keys()
    while True:
        children = {pid for pid, (parent, _, _) in processes.items() if parent in selected}
        expanded = selected | children
        if expanded == selected:
            return selected
        selected = expanded


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--browser', action='append', required=True, metavar='NAME=EXECUTABLE_PREFIX')
    parser.add_argument('--samples', type=int, default=6)
    parser.add_argument('--interval', type=float, default=5)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    if args.samples < 1 or args.interval < 0:
        parser.error('samples must be positive and interval non-negative')
    browsers = dict(item.split('=', 1) for item in args.browser)
    samples = {name: [] for name in browsers}
    counts = {name: [] for name in browsers}
    for sample in range(args.samples):
        processes = process_table()
        for name, prefix in browsers.items():
            pids = tree(processes, prefix)
            if not pids:
                raise SystemExit(f'No processes matched {name}: {prefix}')
            samples[name].append(round(sum(processes[pid][1] for pid in pids) / 1024, 2))
            counts[name].append(len(pids))
        if sample + 1 < args.samples:
            time.sleep(args.interval)
    report = {'metric': 'aggregate process-tree RSS, MiB; shared memory may be counted more than once',
              'workload': 'Record equivalent pages, profiles, extensions, and warm-up alongside this file.',
              'browsers': {name: {'median_mib': round(statistics.median(values), 2),
                                  'samples_mib': values, 'process_counts': counts[name]}
                           for name, values in samples.items()}}
    text = json.dumps(report, indent=2)
    if args.output:
        args.output.write_text(text + '\n')
    print(text)


if __name__ == '__main__':
    main()
