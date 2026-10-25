#!/usr/bin/env python3
"""Build the pinned macOS CEF runtime with H.264/AAC enabled.

Upstream recommends 200 GB free. This release-only build omits symbols, LTO,
PGO profiles and archives to reduce space, and stops below a 20 GiB reserve.
Does not replace a running app or change the stock CEF cache.
"""
import argparse
import json
import os
import platform
import re
import signal
import shlex
import shutil
import subprocess
import sys
import time
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
VERSION = '156.0.8+g426920c+chromium-156.0.8078.48'
COMMIT = '426920c33ef22551e0b8f48cf343d623ac25642d'
BRANCH = '8078'
GN = ('is_official_build=true is_component_build=false symbol_level=0 '
      'use_thin_lto=false chrome_pgo_phase=0 proprietary_codecs=true ffmpeg_branding=Chrome')


def run(*args, **kwargs):
    print('+', shlex.join(map(str, args)), flush=True)
    return subprocess.run(list(map(str, args)), check=True, **kwargs)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--build-dir', type=Path, required=True)
    parser.add_argument('--check', action='store_true', help='check prerequisites without downloading or building')
    args = parser.parse_args()
    lock = tomllib.loads((ROOT / 'Cargo.lock').read_text())
    pinned = next(p['version'] for p in lock['package'] if p['name'] == 'cef-dll-sys')
    if pinned != '156.1.0+156.0.8':
        parser.error('Update the CEF source pin in this script to match Cargo.lock first')
    if sys.platform != 'darwin' or platform.machine() not in ('arm64', 'x86_64'):
        parser.error('Run this build on a macOS arm64 or x86_64 host')
    build = args.build_dir.expanduser().resolve()
    # automate-git.py builds shell commands internally; refuse paths it cannot quote safely.
    if not re.fullmatch(r'/[A-Za-z0-9_./-]+', str(build)):
        parser.error('Use a build directory containing only letters, numbers, /, _, -, and .')
    ancestor = build
    while not ancestor.exists():
        ancestor = ancestor.parent
    available = shutil.disk_usage(ancestor).free / 1024**3
    print(f'CEF {VERSION}\nGN_DEFINES={GN}\nBuild directory: {build}\nFree space: {available:.1f} GiB', flush=True)
    starting = not (build / 'chromium/src/.git').exists()
    minimum = 100 if starting else 20
    if available < minimum:
        parser.error(f'This build needs at least {minimum} GiB free to {"start" if starting else "resume"}; this volume has {available:.1f} GiB.')
    if available < 200:
        print('Below upstream\'s recommended 200 GB. Trying a reduced release build with a 20 GiB reserve.', flush=True)
    for tool in ('git', 'curl', 'xcodebuild', 'xcrun'):
        if not shutil.which(tool):
            parser.error(f'Missing prerequisite: {tool}')
    run('xcodebuild', '-version')
    run('xcrun', '--sdk', 'macosx', '--show-sdk-version')
    metal = subprocess.run(['xcrun', '--sdk', 'macosx', 'metal', '--version'], capture_output=True, text=True)
    if metal.returncode:
        parser.error('Metal compiler unavailable. Install it with: xcodebuild -downloadComponent MetalToolchain')
    if args.check:
        return
    runtime = build / 'runtime'
    if runtime.exists():
        parser.error(f'{runtime} already exists; choose a new build directory or reuse that runtime with CEF_PATH')
    build.mkdir(parents=True, exist_ok=True)
    driver = build / 'automate-git.py'
    # Fetch from the exact CEF revision used by the Rust bindings, with TLS verification.
    run('curl', '--fail', '--location', '--proto', '=https', '--proto-redir', '=https',
        '--output', driver, f'https://raw.githubusercontent.com/chromiumembedded/cef/{COMMIT}/tools/automate/automate-git.py')
    arch = 'arm64' if platform.machine() == 'arm64' else 'x64'
    command = ['/usr/bin/nice', '-n', '10', sys.executable, str(driver), f'--download-dir={build}', f'--branch={BRANCH}',
               f'--checkout={COMMIT}', f'--{arch}-build', '--no-chromium-history',
               '--no-debug-build', '--minimal-distrib-only', '--no-distrib-symbols',
               '--no-distrib-docs', '--no-distrib-archive', '--force-build', '--force-distrib']
    print('+', shlex.join(command), flush=True)
    process = subprocess.Popen(command, env={**os.environ, 'GN_DEFINES': GN, 'PYTHONUNBUFFERED': '1'}, start_new_session=True)
    try:
        while process.poll() is None:
            if shutil.disk_usage(build).free < 20 * 1024**3:
                raise RuntimeError('Stopping CEF build to preserve 20 GiB free. Sources remain for a later retry.')
            time.sleep(2)
        if process.returncode:
            raise subprocess.CalledProcessError(process.returncode, command)
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
    candidates = list((build / 'chromium/src/cef/binary_distrib').glob(f'cef_binary_{VERSION}_macos{arch}_minimal'))
    if len(candidates) != 1:
        raise RuntimeError(f'Expected exactly one matching minimal distribution, found {candidates}')
    source = candidates[0]
    header = (source / 'include/cef_version.h').read_text()
    if f'#define CEF_VERSION "{VERSION}"' not in header or COMMIT not in header:
        raise RuntimeError('Built CEF does not match the pinned version and commit')
    # cef-rs expects a flattened Release directory, rather than CEF's raw distribution layout.
    staging = build / 'runtime-staging'
    if staging.exists():
        raise RuntimeError(f'Remove or inspect the incomplete previous output: {staging}')
    shutil.copytree(source / 'Release', staging, symlinks=True)
    for name in ('CMakeLists.txt', 'cmake', 'include', 'libcef_dll', 'CREDITS.html'):
        path = source / name
        if path.is_dir():
            shutil.copytree(path, staging / name, symlinks=True)
        else:
            shutil.copy2(path, staging / name)
    # Local source build, not an archive downloaded from Spotify. Keep that provenance explicit.
    (staging / 'archive.json').write_text(json.dumps({
        'type': 'minimal', 'name': source.name, 'sha1': '',
        'source_commit': COMMIT, 'gn_defines': GN,
    }, indent=2) + '\n')
    staging.rename(runtime)
    print('\nRuntime ready. Build and verify playback with:\n'
          f'CEF_PATH={shlex.quote(str(runtime))} ./tools/build-chromium.sh --release\n'
          'python3 tools/test_media_playback.py', flush=True)


if __name__ == '__main__':
    main()
