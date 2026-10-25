"""Client for a running browser's loopback API.

Every route except /shell requires the per-launch token in the x-atrium-token
header. It is taken from --token-file, else <--data-dir>/api-token, else the
ATRIUM_API_TOKEN environment variable. Files are re-read on every request, so a
restarted instance's new token is picked up.
"""
import json
import os
import urllib.error
import urllib.request
from pathlib import Path

TOKEN_HEADER = 'x-atrium-token'


class MissingToken(urllib.error.URLError):
    """Raised until the instance has written its token, so readiness loops keep waiting."""


def add_arguments(parser, api='http://127.0.0.1:7791'):
    parser.add_argument('--api', default=api, help='base URL of the browser API (default: %(default)s)')
    parser.add_argument('--data-dir', type=Path, help="the instance's --data-dir; its api-token file is used")
    parser.add_argument('--token-file', type=Path, help='an api-token file to use')


def token(token_file=None, data_dir=None):
    path = token_file or (data_dir / 'api-token' if data_dir else None)
    if path:
        try:
            value = path.read_text().strip()
        except FileNotFoundError:
            value = ''
        if value:
            return value
    value = os.environ.get('ATRIUM_API_TOKEN', '').strip()
    if value:
        return value
    raise MissingToken('No API token: pass --data-dir or --token-file, or set ATRIUM_API_TOKEN')


def instance_env():
    """Environment for test instances: no login-keychain prompt, window kept out of the user's way."""
    return {**os.environ, 'ATRIUM_MOCK_KEYCHAIN': '1', 'ATRIUM_BACKGROUND_WINDOW': '1'}


class Client:
    def __init__(self, base, token_file=None, data_dir=None):
        self.base = base.rstrip('/')
        self.token_file = token_file
        self.data_dir = data_dir

    @classmethod
    def from_args(cls, args):
        return cls(args.api, args.token_file, args.data_dir)

    def headers(self, extra=None):
        return {'Content-Type': 'application/json', TOKEN_HEADER: token(self.token_file, self.data_dir), **(extra or {})}

    def request(self, path, data=None, timeout=35, headers=None):
        body = None if data is None else json.dumps(data).encode()
        req = urllib.request.Request(self.base + path, data=body, headers=self.headers(headers))
        with urllib.request.urlopen(req, timeout=timeout) as response:
            return json.load(response)

    def api(self, path, data=None, timeout=35):
        """Request JSON and fail on an application-level error."""
        result = self.request(path, data, timeout)
        assert not (isinstance(result, dict) and 'error' in result), result
        return result

    def tool(self, name, arguments, timeout=90):
        """Agent tools wait for loads and page snapshots, which can exceed half a minute on a loaded machine."""
        result = self.request('/api/tool', {'name': name, 'args': arguments}, timeout)
        assert not result.get('isError'), result
        return result['content']

    def command(self, **data):
        return self.api('/api/browser/command', data)
