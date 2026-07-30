#!/usr/bin/env python3
"""Discard multipart uploads for rgfile loopback benchmarks."""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import socket
import time


FILE_ID = "0123abcd-000000example"


class UploadSink(ThreadingHTTPServer):
    daemon_threads = True

    def server_bind(self):
        super().server_bind()
        self.socket.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        if self.path == "/":
            host, port = self.server.server_address
            body = f'<script>var server = "http://{host}:{port}";</script>'.encode()
            self._reply(200, "text/html", body)
            return
        if self.path == "/metrics":
            with self.server.metrics_lock:
                body = json.dumps(self.server.metrics).encode()
            self._reply(200, "application/json", body)
            return
        if self.path == "/reset":
            with self.server.metrics_lock:
                self.server.metrics = empty_metrics()
            self._reply(200, "application/json", b'{"status":"ok"}')
            return
        self._reply(404, "text/plain", b"")

    def do_POST(self):
        if self.path != "/upload_chunk.php":
            self._reply(404, "text/plain", b"")
            return
        content_length = self.headers.get("Content-Length")
        if content_length is None:
            self._reply(411, "text/plain", b"")
            return

        remaining = int(content_length)
        started_wall_ns = time.time_ns()
        started_ns = time.monotonic_ns()
        while remaining:
            data = self.rfile.read(min(remaining, 1024 * 1024))
            if not data:
                self.close_connection = True
                return
            remaining -= len(data)
        finished_ns = time.monotonic_ns()

        with self.server.metrics_lock:
            if self.server.metrics["first_request_wall_ns"] == 0:
                self.server.metrics["first_request_wall_ns"] = started_wall_ns
            self.server.metrics["requests"] += 1
            self.server.metrics["wire_bytes"] += int(content_length)
            self.server.metrics["last_request_ns"] = finished_ns - started_ns

        host, port = self.server.server_address
        body = json.dumps(
            {
                "status": 0,
                "url": f"http://{host}:{port}/{FILE_ID}",
            },
            separators=(",", ":"),
        ).encode()
        self._reply(200, "application/json", body)

    def _reply(self, status, content_type, body):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, _format, *_args):
        return


def empty_metrics():
    return {
        "requests": 0,
        "wire_bytes": 0,
        "last_request_ns": 0,
        "first_request_wall_ns": 0,
    }


if __name__ == "__main__":
    import threading

    server = UploadSink(("127.0.0.1", 0), Handler)
    server.metrics_lock = threading.Lock()
    server.metrics = empty_metrics()
    host, port = server.server_address
    print(f"http://{host}:{port}/", flush=True)
    server.serve_forever()
