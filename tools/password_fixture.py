#!/usr/bin/env python3
"""Local login, signup, and WebAuthn pages for password and passkey tests. No external services."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse
import argparse
import threading

PAGES = {
    '/login': '''<h1>Sign in</h1>
<form id="login" method="post" action="/welcome">
  <label>Email <input id="username" name="username" autocomplete="username"></label>
  <label>Password <input id="password" name="password" type="password" autocomplete="current-password"></label>
  <button type="submit">Sign in</button>
</form>''',
    '/signup': '''<h1>Create your account</h1>
<form id="signup" method="post" action="/welcome">
  <label>Email <input id="username" name="username" type="email" autocomplete="username"></label>
  <label>New password <input id="new" name="new" type="password" autocomplete="new-password"></label>
  <label>Confirm <input id="confirm" name="confirm" type="password" autocomplete="new-password"></label>
  <button type="submit">Create account</button>
</form>''',
    '/spa-login': '''<h1>App sign in</h1>
<div id="app"><input id="username" placeholder="Username"><input id="password" type="password" placeholder="Password">
<button id="go" type="button">Log in</button></div>
<script>document.querySelector('#go').onclick = () => { document.querySelector('#app').innerHTML = '<p id="done">Welcome back</p>'; };</script>''',
    '/welcome': '<h1 id="welcome">Signed in</h1>',
    '/passkey': '''<h1>Passkeys</h1><p id="status">Ready</p>
<script>
const enc = new TextEncoder(), dec = new TextDecoder();
const b64url = (buf) => btoa(String.fromCharCode(...new Uint8Array(buf))).replace(/\\+/g, '-').replace(/\\//g, '_').replace(/=+$/, '');
const concat = (...parts) => { const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0)); let i = 0; for (const p of parts) { out.set(p, i); i += p.length; } return out; };
// WebAuthn ES256 signatures are DER; WebCrypto expects r || s.
function derToRaw(der) {
  let i = 2; if (der[1] & 0x80) i += der[1] & 0x7f;
  const parts = [];
  for (let k = 0; k < 2; k++) {
    i++; const len = der[i++]; let v = der.slice(i, i + len); i += len;
    while (v.length > 32 && v[0] === 0) v = v.slice(1);
    const p = new Uint8Array(32); p.set(v, 32 - v.length); parts.push(p);
  }
  return concat(...parts);
}
window.passkey = {
  stored: null,
  async capabilities() {
    return {
      publicKeyCredential: typeof PublicKeyCredential === 'function',
      secureContext: isSecureContext,
      platformAuthenticator: await PublicKeyCredential.isUserVerifyingPlatformAuthenticatorAvailable(),
      conditionalMediation: await PublicKeyCredential.isConditionalMediationAvailable?.() ?? null,
    };
  },
  async register() {
    const challenge = crypto.getRandomValues(new Uint8Array(32));
    const cred = await navigator.credentials.create({ publicKey: {
      rp: { name: 'Aside fixture', id: location.hostname },
      user: { id: enc.encode('user-ada'), name: 'ada@example.test', displayName: 'Ada' },
      challenge, pubKeyCredParams: [{ type: 'public-key', alg: -7 }],
      authenticatorSelection: { residentKey: 'required', userVerification: 'required' }, timeout: 10000,
    }, signal: AbortSignal.timeout(15000) });
    const client = JSON.parse(dec.decode(cred.response.clientDataJSON));
    this.stored = { id: cred.id, spki: cred.response.getPublicKey() };
    return { id: cred.id, type: client.type, origin: client.origin, challengeOk: client.challenge === b64url(challenge),
      alg: cred.response.getPublicKeyAlgorithm(), attachment: cred.authenticatorAttachment };
  },
  async login() {
    const challenge = crypto.getRandomValues(new Uint8Array(32));
    const a = await navigator.credentials.get({ publicKey: { challenge, rpId: location.hostname, userVerification: 'required', timeout: 10000 },
      signal: AbortSignal.timeout(15000) });
    const client = JSON.parse(dec.decode(a.response.clientDataJSON));
    const key = await crypto.subtle.importKey('spki', this.stored.spki, { name: 'ECDSA', namedCurve: 'P-256' }, false, ['verify']);
    const data = new Uint8Array(a.response.authenticatorData);
    const signed = concat(data, new Uint8Array(await crypto.subtle.digest('SHA-256', a.response.clientDataJSON)));
    const valid = await crypto.subtle.verify({ name: 'ECDSA', hash: 'SHA-256' }, key, derToRaw(new Uint8Array(a.response.signature)), signed);
    return { id: a.id, sameCredential: a.id === this.stored.id, type: client.type, origin: client.origin,
      challengeOk: client.challenge === b64url(challenge), signatureValid: valid,
      userHandle: dec.decode(a.response.userHandle), userVerified: !!(data[32] & 4) };
  },
};
</script>''',
}


class Handler(BaseHTTPRequestHandler):
    def page(self):
        path = urlparse(self.path).path
        body = PAGES.get(path)
        if body is None:
            self.send_error(404)
            return
        html = f'<!doctype html><meta charset="utf-8"><title>Fixture {path}</title>{body}'.encode()
        self.send_response(200)
        self.send_header('Content-Type', 'text/html; charset=utf-8')
        self.send_header('Cache-Control', 'no-store')
        self.send_header('Content-Length', str(len(html)))
        self.end_headers()
        self.wfile.write(html)

    def do_GET(self):
        self.page()

    def do_POST(self):
        self.rfile.read(int(self.headers.get('Content-Length') or 0))
        self.page()

    def log_message(self, *_):
        pass


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--port', type=int, action='append', help='repeat to serve several origins')
    args = parser.parse_args()
    servers = [ThreadingHTTPServer(('127.0.0.1', port), Handler) for port in (args.port or [7831])]
    for server in servers[1:]:
        threading.Thread(target=server.serve_forever, daemon=True).start()
    print(' '.join(f'http://127.0.0.1:{s.server_address[1]}' for s in servers), flush=True)
    servers[0].serve_forever()
