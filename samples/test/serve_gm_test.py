#!/usr/bin/env python3
"""Local test server for samples/test/gm-test.user.js.

Serves samples/ on 127.0.0.1:8099 with the strict security policy a site
like X uses, a /slow address that never answers in time, and a 3 MB file
that has to come back in several pieces.

    python samples/test/serve_gm_test.py
"""
import http.server
import os
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIG = bytes(range(256)) * 12000
CSP = "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'"


class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **k):
        super().__init__(*a, directory=ROOT, **k)

    def end_headers(self):
        if self.path.split("?")[0].endswith(".html"):
            self.send_header("Content-Security-Policy", CSP)
        super().end_headers()

    def do_GET(self):
        if self.path == "/slow":
            time.sleep(4)
            self.send_response(200)
            self.end_headers()
            return
        if self.path == "/test/big.bin":
            self.send_response(200)
            self.send_header("Content-Length", str(len(BIG)))
            self.end_headers()
            self.wfile.write(BIG)
            return
        super().do_GET()

    def log_message(self, *a):
        pass


if __name__ == "__main__":
    http.server.ThreadingHTTPServer(("127.0.0.1", 8099), Handler).serve_forever()
