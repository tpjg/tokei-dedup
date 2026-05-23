"""HTTP server with a shared health-check pattern."""

import http.server
import socketserver


def start_server(port):
    handler = http.server.SimpleHTTPRequestHandler
    with socketserver.TCPServer(("", port), handler) as httpd:
        print(f"Serving on {port}")
        httpd.serve_forever()


def parse_args(argv):
    """Tiny argument parser — same shape as cli.py::read_cli_args (Type-2 clone)."""
    out = {}
    i = 0
    while i < len(argv):
        token = argv[i]
        if token.startswith("--"):
            key = token[2:]
            value = argv[i + 1] if i + 1 < len(argv) else True
            out[key] = value
            i += 2
        else:
            out.setdefault("positional", []).append(token)
            i += 1
    return out


def unrelated_helper(x, y):
    return x * y + (x ** 2)
