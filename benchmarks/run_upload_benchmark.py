#!/usr/bin/env python3
"""Run and summarize the release-mode loopback upload benchmark."""

import argparse
import json
from pathlib import Path
import statistics
import subprocess
import tempfile
import time
from urllib.request import urlopen


def parse_args():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--file", type=Path)
    parser.add_argument("--size", type=int, default=1024 * 1024 * 1024)
    parser.add_argument("--chunk-size", type=int, default=64 * 1024 * 1024)
    parser.add_argument("--threads", type=int, default=1)
    parser.add_argument("--samples", type=int, default=15)
    parser.add_argument("--warmup", type=int, default=2)
    return parser.parse_args()


def peak_rss_kib(process):
    peak = 0
    status_path = Path(f"/proc/{process.pid}/status")
    while process.poll() is None:
        try:
            for line in status_path.read_text().splitlines():
                if line.startswith("VmRSS:"):
                    peak = max(peak, int(line.split()[1]))
                    break
        except FileNotFoundError:
            pass
        time.sleep(0.005)
    return peak


def run_client(binary, url, source, chunk_size, threads, iterations):
    command = [
        str(binary),
        url,
        str(source),
        str(chunk_size),
        str(threads),
        str(iterations),
    ]
    process = subprocess.Popen(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    peak = peak_rss_kib(process)
    stdout, stderr = process.communicate()
    if process.returncode != 0:
        raise RuntimeError(f"benchmark client failed: {stderr.strip()}")
    return [json.loads(line) for line in stdout.splitlines() if line], peak


def fetch_json(url):
    with urlopen(url, timeout=5) as response:
        return json.load(response)


def main():
    args = parse_args()
    binary = args.binary.resolve()
    if not binary.is_file():
        raise SystemExit(f"benchmark binary does not exist: {binary}")
    if args.samples < 1 or args.warmup < 0:
        raise SystemExit("samples must be positive and warmup must be non-negative")

    with tempfile.TemporaryDirectory(prefix="rgfile-upload-benchmark-") as temporary:
        temporary = Path(temporary)
        source = args.file.resolve() if args.file else temporary / "source.bin"
        if args.file is None:
            with source.open("wb") as output:
                output.truncate(args.size)

        server = subprocess.Popen(
            ["python3", str(Path(__file__).with_name("upload_sink.py"))],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            url = server.stdout.readline().strip()
            if not url:
                stderr = server.stderr.read()
                raise RuntimeError(f"upload sink failed to start: {stderr.strip()}")

            rows, peak = run_client(
                binary,
                url,
                source,
                args.chunk_size,
                args.threads,
                args.samples + args.warmup,
            )
            samples = [
                row["elapsed_ns"] / 1_000_000_000 for row in rows[args.warmup :]
            ]
            median = statistics.median(samples)
            mad = statistics.median(abs(sample - median) for sample in samples)

            fetch_json(f"{url}reset")
            first_started = time.time_ns()
            run_client(binary, url, source, args.chunk_size, args.threads, 1)
            metrics = fetch_json(f"{url}metrics")
            first_post_ms = (
                metrics["first_request_wall_ns"] - first_started
            ) / 1_000_000

            print(
                json.dumps(
                    {
                        "binary": str(binary),
                        "bytes": source.stat().st_size,
                        "chunk_size": args.chunk_size,
                        "threads": args.threads,
                        "samples": len(samples),
                        "warmup": args.warmup,
                        "median_seconds": median,
                        "mad_seconds": mad,
                        "throughput_gib_per_second": (
                            source.stat().st_size / (1024**3) / median
                        ),
                        "peak_rss_kib": peak,
                        "first_post_ms": first_post_ms,
                    },
                    indent=2,
                    sort_keys=True,
                )
            )
        finally:
            server.terminate()
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait()


if __name__ == "__main__":
    main()
