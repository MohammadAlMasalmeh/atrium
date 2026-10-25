#!/usr/bin/env python3
"""Require real H.264/AAC decoding through HTML media and MediaSource in CEF.

Uses generated, silent local fixtures and an isolated background-only app copy.
Exits non-zero for the stock runtime, which lacks H.264/AAC. No external sites.
"""
import argparse
import json
import tempfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from threading import Thread

import background_app

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / 'tools/fixtures/media'
TYPES = {
    'h264': 'video/mp4; codecs="avc1.42C01E"',
    'aac': 'audio/mp4; codecs="mp4a.40.2"',
    'h264-aac': 'video/mp4; codecs="avc1.42C01E, mp4a.40.2"',
}


class Fixture(BaseHTTPRequestHandler):
    def do_GET(self):
        name = self.path.removeprefix('/')
        if name in {key + '.mp4' for key in TYPES}:
            body = (FIXTURES / name).read_bytes()
            mime = 'audio/mp4' if name == 'aac.mp4' else 'video/mp4'
        elif self.path == '/':
            body, mime = b'<!doctype html><title>Local codec playback test</title>', 'text/html'
        else:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header('Content-Type', mime)
        self.send_header('Content-Length', str(len(body)))
        self.send_header('Cache-Control', 'no-store')
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


PLAYBACK = r'''
const media = document.createElement(name === 'aac' ? 'audio' : 'video');
media.muted = true;
media.playsInline = true;
document.body.append(media);
let objectURL;
try {
  const decoded = new Promise((resolve, reject) => {
    media.addEventListener('error', () => reject(new Error('media error ' + media.error?.code)), {once:true});
    media.addEventListener('ended', () => resolve({time:media.currentTime,
      width:media.videoWidth || 0, height:media.videoHeight || 0,
      frames:media.getVideoPlaybackQuality?.().totalVideoFrames || 0}), {once:true});
  });
  let feed = Promise.resolve();
  if (mse) {
    const source = new MediaSource();
    objectURL = URL.createObjectURL(source);
    const opened = new Promise(resolve => source.addEventListener('sourceopen', resolve, {once:true}));
    media.src = objectURL;
    feed = (async () => {
      await opened;
      const buffer = source.addSourceBuffer(type);
      const data = await (await fetch('/' + name + '.mp4')).arrayBuffer();
      await new Promise((resolve, reject) => {
        buffer.addEventListener('updateend', resolve, {once:true});
        buffer.addEventListener('error', () => reject(new Error('SourceBuffer error')), {once:true});
        buffer.appendBuffer(data);
      });
      source.endOfStream();
    })();
  } else {
    media.src = '/' + name + '.mp4';
  }
  const playback = Promise.all([decoded, feed, media.play()]).then(([result]) => result);
  let timer;
  try {
    return await Promise.race([playback, new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error('playback timed out')), 12000);
    })]);
  } finally { clearTimeout(timer); }
} finally {
  media.pause(); media.removeAttribute('src'); media.load(); media.remove();
  if (objectURL) URL.revokeObjectURL(objectURL);
}
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--app', type=Path, default=ROOT / 'target/bundle/Atrium.app')
    parser.add_argument('--port', type=int, default=7853)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='atrium-media-', dir='/private/tmp') as folder:
        work = Path(folder)
        watcher = background_app.FrontWatcher(work)
        server = ThreadingHTTPServer(('127.0.0.1', 0), Fixture)
        Thread(target=server.serve_forever, daemon=True).start()
        instance = None
        try:
            app = background_app.prepare(args.app.resolve(), work / 'Atrium.app')
            instance = background_app.Instance(app, work / 'profile', args.port, work / 'app.log',
                                               {'ATRIUM_DEBUG_API': '1'})
            instance.start()
            pid = instance.wait_ready()
            client = instance.client
            client.tool('open_tab', {'url': f'http://127.0.0.1:{server.server_port}/'})

            def evaluate(code):
                return json.loads(client.tool('evaluate', {'code': code})[0]['text'])

            capabilities = evaluate('const types = ' + json.dumps(TYPES) + ''';
                const media = document.createElement('video');
                return Object.fromEntries(Object.entries(types).map(([name,type]) => [name, {
                  html: media.canPlayType(type), mse: MediaSource.isTypeSupported(type)
                }]));''')
            print('Codec capabilities:', json.dumps(capabilities, indent=2), flush=True)
            missing = [name for name, caps in capabilities.items() if not caps['html'] or not caps['mse']]
            assert not missing, ('Runtime lacks ' + ', '.join(missing)
                                 + '; rebuild with tools/build-cef-media.py and use its CEF_PATH')
            for name, mime in TYPES.items():
                for mse in (False, True):
                    result = evaluate(f'const name={json.dumps(name)}, type={json.dumps(mime)}, mse={json.dumps(mse)};\n' + PLAYBACK)
                    assert result['time'] >= .8, result
                    if name != 'aac':
                        assert result['width'] == 128 and result['height'] == 96 and result['frames'] >= 8, result
                    print(f'PASS: {name} via {"MediaSource" if mse else "HTML media"}: {result}', flush=True)
            instance.quit(pid)
        finally:
            if instance:
                instance.kill_all()
                instance.log.close()
            server.shutdown()
            server.server_close()
            watcher.stop()
            watcher.check()


if __name__ == '__main__':
    main()
