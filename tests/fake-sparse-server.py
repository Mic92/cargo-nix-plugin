#!/usr/bin/env python3
"""Serve a static sparse cargo registry index over loopback.

Used by remote-sparse-test.nix to exercise the plugin's HTTP fallback
inside the nix sandbox, where no outbound network is available.

Usage: fake-sparse-server.py <port-file> <access-log> <docroot>

Writes the kernel-allocated listening port to <port-file> as soon as
the socket is bound, so the caller can block on `[[ -s $PORT_FILE ]]`
instead of sleep-polling. Each request path is appended to
<access-log>, one per line, so the test can assert that the plugin
actually hit the server rather than silently reading a local cache.
"""

import http.server
import os
import socketserver
import sys


def main() -> None:
    port_file, access_log, docroot = sys.argv[1:4]

    class Handler(http.server.SimpleHTTPRequestHandler):
        def __init__(self, *a, **kw):
            super().__init__(*a, directory=docroot, **kw)

        def log_message(self, fmt, *args):
            with open(access_log, "a") as f:
                f.write(self.path + "\n")

    # Avoids TIME_WAIT races when rebuilding in a tight loop.
    socketserver.TCPServer.allow_reuse_address = True

    with socketserver.TCPServer(("127.0.0.1", 0), Handler) as httpd:
        port = httpd.server_address[1]
        # Atomic write so the reader sees either nothing or the full port.
        fd = os.open(port_file, os.O_WRONLY | os.O_TRUNC)
        os.write(fd, str(port).encode())
        os.close(fd)
        httpd.serve_forever()


if __name__ == "__main__":
    main()
