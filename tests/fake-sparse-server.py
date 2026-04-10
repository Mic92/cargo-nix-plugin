#!/usr/bin/env python3
"""Serve a static sparse cargo registry index over loopback.

Used by remote-sparse-test.nix to exercise the plugin's HTTP fallback
inside the nix sandbox, where no outbound network is available.

Usage: fake-sparse-server.py <port-file> <access-log> <docroot> [--fail-mode MODE]

Writes the kernel-allocated listening port to <port-file> as soon as
the socket is bound, so the caller can block on `[[ -s $PORT_FILE ]]`
instead of sleep-polling. Each request path is appended to
<access-log>, one per line, so the test can assert that the plugin
actually hit the server rather than silently reading a local cache.

Fail modes (for retry tests):

  503-then-ok:N           First N requests *per path* return 503,
                          subsequent requests serve the file normally.
  429-with-retry-after:N  First N requests *per path* return 429 with
                          `Retry-After: 1`; subsequent requests serve
                          normally. Verifies the plugin honors the header
                          and the loop completes within budget.
  corrupt-body:N          First N requests *per path* return a malformed
                          body (truncated JSON) with 200 OK; subsequent
                          requests serve normally. Mirrors the
                          asn1-rs production failure.

Failure counters are per-path so that retry-test.nix can assert each
crate independently saw N failures plus a success, regardless of how
many crates the plugin prefetches concurrently.
"""

import argparse
import http.server
import os
import socketserver
import ssl


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("port_file")
    parser.add_argument("access_log")
    parser.add_argument("docroot")
    parser.add_argument("--fail-mode", default=None)
    parser.add_argument(
        "--tls-cert",
        default=None,
        help="serve over HTTPS with this PEM cert (key in --tls-key)",
    )
    parser.add_argument("--tls-key", default=None)
    args = parser.parse_args()

    fail_kind: str | None = None
    fail_count: int = 0
    if args.fail_mode:
        kind, _, count_str = args.fail_mode.partition(":")
        fail_kind = kind
        fail_count = int(count_str) if count_str else 0
    fail_remaining: dict[str, int] = {}

    class Handler(http.server.SimpleHTTPRequestHandler):
        def __init__(self, *a, **kw):
            super().__init__(*a, directory=args.docroot, **kw)

        def log_message(self, format, *log_args):  # noqa: A002 — base class param name
            del format, log_args
            with open(args.access_log, "a") as f:
                f.write(self.path + "\n")

        def _serve_config(self) -> None:
            # config.json's `dl` URL template needs the live port.
            # Fixtures use the literal token `PORT`; substitute on the fly
            # so callers don't have to copy-and-sed the docroot.
            with open(os.path.join(args.docroot, "config.json"), "rb") as f:
                body = f.read().replace(b"PORT", str(bound_port).encode())
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            if self.path == "/config.json":
                return self._serve_config()
            remaining = fail_remaining.setdefault(self.path, fail_count)
            if fail_kind and remaining > 0:
                fail_remaining[self.path] = remaining - 1
                if fail_kind == "503-then-ok":
                    self.send_response(503)
                    self.end_headers()
                    self.wfile.write(b"transient upstream error\n")
                    return
                elif fail_kind == "429-with-retry-after":
                    self.send_response(429)
                    self.send_header("Retry-After", "1")
                    self.end_headers()
                    self.wfile.write(b"too many requests\n")
                    return
                elif fail_kind == "corrupt-body":
                    self.send_response(200)
                    self.send_header("Content-Type", "text/plain")
                    self.end_headers()
                    # Truncated JSON — what tame-index sees as
                    # "expected `:` at line 1 column N".
                    self.wfile.write(b'{"name":"serde","vers":"1.0.228","de\n')
                    return
            super().do_GET()

    # Avoids TIME_WAIT races when rebuilding in a tight loop.
    socketserver.TCPServer.allow_reuse_address = True

    bound_port = 0
    with socketserver.TCPServer(("127.0.0.1", 0), Handler) as httpd:
        bound_port = httpd.server_address[1]
        if args.tls_cert:
            ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            ctx.load_cert_chain(args.tls_cert, args.tls_key)
            httpd.socket = ctx.wrap_socket(httpd.socket, server_side=True)
        port = bound_port
        # Atomic write so the reader sees either nothing or the full port.
        fd = os.open(args.port_file, os.O_WRONLY | os.O_TRUNC)
        os.write(fd, str(port).encode())
        os.close(fd)
        httpd.serve_forever()


if __name__ == "__main__":
    main()
