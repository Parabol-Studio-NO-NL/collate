#!/usr/bin/env python3
"""Run a local Cargo command with disk-space checks on macOS and Linux."""

import argparse
from contextlib import suppress
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time


GIB = 1024**3
ROOT = Path(__file__).resolve().parents[2]


def positive_gib(value):
    gib = int(value)
    if gib < 1:
        raise argparse.ArgumentTypeError("enter a whole number of GiB greater than zero")
    return gib * GIB


def cache_paths():
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"], cwd=ROOT,
    ))
    target = Path(metadata["target_directory"]).resolve()
    build = Path(metadata.get("build_directory", target)).resolve()
    return list(dict.fromkeys([target, build]))


def cache_size(paths):
    total = 0
    for path in paths:
        if path.is_dir() and not any(path != other and path.is_relative_to(other) for other in paths):
            total += int(subprocess.check_output(["du", "-sk", path]).split()[0]) * 1024
    return total


def ensure_space(paths, minimum):
    available = []
    for path in paths:
        for parent in (path, *path.parents):
            if parent.exists():
                free = shutil.disk_usage(parent).free
                if free < minimum:
                    raise RuntimeError(f"{parent} has {free / GIB:.1f} GiB free. This build needs a reserve of {minimum / GIB:g} GiB.")
                available.append(free)
                break
    return min(available)


def run_cargo(command, paths, minimum):
    process = subprocess.Popen(["cargo", *command], cwd=ROOT, start_new_session=True)
    try:
        while True:
            try:
                return process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                ensure_space(paths, minimum)
    finally:
        if process.poll() is None:
            with suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                with suppress(ProcessLookupError):
                    os.killpg(process.pid, signal.SIGKILL)
                process.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--min-free-gib", type=positive_gib, default=5 * GIB, help="stop below this free-space reserve (default 5 GiB)")
    parser.add_argument("--warn-cache-gib", type=positive_gib, default=12 * GIB, help="warn above this cache size (default 12 GiB)")
    parser.add_argument("command", choices=("build", "check", "test", "clippy"))
    parser.add_argument("cargo_args", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    for arg in args.cargo_args:
        if arg == "--":
            break
        if arg.partition("=")[0] in {"--target-dir", "--manifest-path", "--config"}:
            parser.error("set Cargo output directories through the environment or Cargo configuration")

    paths = cache_paths()
    volumes = [ROOT, *paths, Path(tempfile.gettempdir())]
    free = ensure_space(volumes, args.min_free_gib)
    before = cache_size(paths)
    print(f"Cargo cache: {before / GIB:.1f} GiB ({', '.join(map(str, paths))}). "
          f"Least free space across build volumes: {free / GIB:.1f} GiB.", flush=True)
    started = time.monotonic()
    try:
        result = run_cargo([args.command, "--locked", *args.cargo_args], volumes, args.min_free_gib)
        free = ensure_space(volumes, args.min_free_gib)
        print(f"Least free space after Cargo: {free / GIB:.1f} GiB.", flush=True)
        return result if result >= 0 else 128 - result
    finally:
        after = cache_size(paths)
        print(f"Cargo cache after {time.monotonic() - started:.1f}s: {after / GIB:.1f} GiB ({(after - before) / GIB:+.1f} GiB).", flush=True)
        if after > args.warn_cache_gib:
            print(f"Cargo cache exceeds {args.warn_cache_gib / GIB:g} GiB. Consider cleaning an unused profile or target.", file=sys.stderr)


def interrupt(signum, _frame):
    raise SystemExit(128 + signum)


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, interrupt)
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)
    except (RuntimeError, OSError, subprocess.SubprocessError) as error:
        sys.exit(f"Local Cargo build: {error}")
