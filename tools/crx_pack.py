#!/usr/bin/env python3
"""Build a CRX3 package signed with an RSA key, using only the openssl command.

    python3 tools/crx_pack.py SRC_DIR KEY.pem OUT.crx

KEY.pem is created if it does not exist. Prints the extension ID.
"""
import hashlib
import io
import subprocess
import sys
import zipfile
from pathlib import Path


def _varint(n):
    out = bytearray()
    while True:
        byte = n & 0x7f
        n >>= 7
        out.append(byte | (0x80 if n else 0))
        if not n:
            return bytes(out)


def _field(number, data):
    return _varint(number << 3 | 2) + _varint(len(data)) + data


def ensure_key(key):
    key = Path(key)
    if not key.exists():
        subprocess.run(['openssl', 'genpkey', '-algorithm', 'RSA', '-pkeyopt', 'rsa_keygen_bits:2048', '-out', str(key)],
                       check=True, capture_output=True)
    return key


def public_key(key):
    return subprocess.run(['openssl', 'pkey', '-in', str(key), '-pubout', '-outform', 'DER'],
                          check=True, capture_output=True).stdout


def extension_id(public):
    return ''.join(chr(97 + (b >> 4)) + chr(97 + (b & 15)) for b in hashlib.sha256(public).digest()[:16])


def zip_dir(src):
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, 'w', zipfile.ZIP_DEFLATED) as archive:
        for path in sorted(Path(src).rglob('*')):
            if path.is_file():
                archive.write(path, path.relative_to(src).as_posix())
    return buffer.getvalue()


def pack(src, key):
    key = ensure_key(key)
    public = public_key(key)
    archive = zip_dir(src)
    signed_data = _field(1, hashlib.sha256(public).digest()[:16])
    message = b'CRX3 SignedData\x00' + len(signed_data).to_bytes(4, 'little') + signed_data + archive
    signature = subprocess.run(['openssl', 'dgst', '-sha256', '-sign', str(key)], input=message,
                               check=True, capture_output=True).stdout
    header = _field(2, _field(1, public) + _field(2, signature)) + _field(10000, signed_data)
    return b'Cr24' + (3).to_bytes(4, 'little') + len(header).to_bytes(4, 'little') + header + archive, extension_id(public)


if __name__ == '__main__':
    crx, ext_id = pack(sys.argv[1], sys.argv[2])
    Path(sys.argv[3]).write_bytes(crx)
    print(ext_id)
