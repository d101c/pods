#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""Serves the files of one directory on 127.0.0.1, slowly, for the engine
install of the headless test: the extension downloads its "release" from here.

    serve.py DIR PORTFILE [BYTES_PER_SECOND]

Writes the port it listens on to PORTFILE. While DIR/tamper exists, the last
byte of every file is changed on the way (a corrupt download). Every request
is printed as "GET <path> <status>".
"""
import http.server
import pathlib
import sys
import time

ROOT = pathlib.Path(sys.argv[1])
RATE = int(sys.argv[3]) if len(sys.argv) > 3 else 0
CHUNK = 16 * 1024


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):  # noqa: N802 - the http.server name
        path = ROOT / self.path.lstrip('/').split('?')[0]
        if not path.is_file() or ROOT not in path.resolve().parents:
            self.send_error(404)
            print('GET', self.path, 404, flush=True)
            return
        data = bytearray(path.read_bytes())
        if (ROOT / 'tamper').exists():
            data[-1] ^= 0xFF
        self.send_response(200)
        self.send_header('Content-Type', 'application/gzip')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        for i in range(0, len(data), CHUNK):
            self.wfile.write(data[i:i + CHUNK])
            if RATE:
                time.sleep(CHUNK / RATE)
        print('GET', self.path, 200, flush=True)

    def log_message(self, *_):
        pass


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
pathlib.Path(sys.argv[2]).write_text(str(server.server_address[1]))
server.serve_forever()
