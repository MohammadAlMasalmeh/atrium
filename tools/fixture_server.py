#!/usr/bin/env python3
"""Local, deterministic browser workloads. No external services or personal data.

/page/*  12 MiB touched buffer, button, textarea (/draft/* pre-fills the textarea)
/form/*  several text fields for typing tests
/scroll/* a long styled page for scroll timing
/video/* a muted, looping local video (VP8 WebM made with ffmpeg when available,
         otherwise an animated canvas stream)
"""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlparse
import argparse
import shutil
import subprocess

CLIP = Path('/private/tmp/aside-fixture-clip.webm')


def make_clip():
    if CLIP.exists() and CLIP.stat().st_size > 0:
        return True
    if not shutil.which('ffmpeg'):
        return False
    partial = CLIP.with_suffix('.partial.webm')
    result = subprocess.run(['ffmpeg', '-y', '-loglevel', 'error', '-f', 'lavfi', '-i', 'testsrc2=size=640x360:rate=30',
                             '-t', '10', '-c:v', 'libvpx', '-b:v', '800k', '-an', str(partial)])
    if result.returncode != 0:
        return False
    partial.rename(CLIP)
    return True


def page(path):
    draft = 'draft' in path
    return f'''<!doctype html><meta charset="utf-8"><title>Fixture {path}</title>
        <h1>Chromium fixture</h1><button id="increment">Increment</button><output id="count">0</output>
        <label>Draft<textarea id="draft"></textarea></label><a href="/next">Next page</a>
        <script>
        window.fixtureMemory = new Uint8Array(12 * 1024 * 1024).fill(42);
        let count = 0; document.querySelector('#increment').onclick = () => document.querySelector('#count').value = ++count;
        {'document.querySelector("#draft").value = "Unsaved test draft";' if draft else ''}
        </script>'''


def form(path):
    fields = ''.join(f'<p><label>Field {i} <input id="field{i}" name="field{i}"></label></p>' for i in range(6))
    return f'''<!doctype html><meta charset="utf-8"><title>Fixture {path}</title>
        <h1>Form fixture</h1><form onsubmit="return false">{fields}
        <p><label>Notes <textarea id="notes" rows="6" cols="60"></textarea></label></p></form>'''


def scroll(path):
    rows = ''.join(f'<section class="card"><h2>Section {i}</h2><p>{"Deterministic scrolling text. " * 40}</p>'
                   f'<div class="swatch" style="background:hsl({i * 37 % 360} 60% 70%)"></div></section>' for i in range(240))
    return f'''<!doctype html><meta charset="utf-8"><title>Fixture {path}</title>
        <style>body{{font:15px system-ui;margin:0 auto;max-width:860px}}.card{{border:1px solid #ccc;border-radius:10px;
        margin:14px;padding:12px;box-shadow:0 2px 8px #0002}}.swatch{{height:90px;border-radius:6px}}</style>
        <h1>Scroll fixture</h1>{rows}'''


def video(path, have_clip):
    source = ('<video id="video" src="/media/clip.webm" muted autoplay loop playsinline width="640" height="360"></video>'
              if have_clip else
              '''<canvas id="canvas" width="640" height="360" hidden></canvas>
              <video id="video" muted autoplay playsinline width="640" height="360"></video><script>
              const c = document.querySelector('#canvas'), g = c.getContext('2d'); let f = 0;
              setInterval(() => { g.fillStyle = `hsl(${f++ % 360} 70% 50%)`; g.fillRect(0, 0, 640, 360);
                g.fillStyle = '#fff'; g.font = '48px sans-serif'; g.fillText('frame ' + f, 40, 200); }, 33);
              document.querySelector('#video').srcObject = c.captureStream(30);</script>''')
    return f'''<!doctype html><meta charset="utf-8"><title>Fixture {path}</title>
        <h1>Video fixture</h1>{source}<script>document.querySelector('#video').play().catch(() => {{}});</script>'''


class Handler(BaseHTTPRequestHandler):
    have_clip = False

    def do_GET(self):
        path = urlparse(self.path).path
        if path == '/media/clip.webm' and self.have_clip:
            return self.send_media()
        if path.startswith('/form/'):
            body = form(path)
        elif path.startswith('/scroll/'):
            body = scroll(path)
        elif path.startswith('/video/'):
            body = video(path, self.have_clip)
        else:
            body = page(path)
        self.send_body(200, 'text/html; charset=utf-8', body.encode())

    def send_body(self, status, kind, body, extra=()):
        self.send_response(status)
        self.send_header('Content-Type', kind)
        self.send_header('Cache-Control', 'no-store')
        self.send_header('Content-Length', str(len(body)))
        for name, value in extra:
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(body)

    def send_media(self):
        data = CLIP.read_bytes()
        spec = self.headers.get('Range', '')
        if spec.startswith('bytes='):
            start_text, _, end_text = spec[6:].partition('-')
            start = int(start_text or 0)
            end = min(int(end_text) if end_text else len(data) - 1, len(data) - 1)
            return self.send_body(206, 'video/webm', data[start:end + 1],
                                  [('Accept-Ranges', 'bytes'), ('Content-Range', f'bytes {start}-{end}/{len(data)}')])
        self.send_body(200, 'video/webm', data, [('Accept-Ranges', 'bytes')])

    def log_message(self, *_):
        pass


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--port', type=int, default=7789)
    args = parser.parse_args()
    Handler.have_clip = make_clip()
    print(f'http://127.0.0.1:{args.port}', flush=True)
    ThreadingHTTPServer(('127.0.0.1', args.port), Handler).serve_forever()
