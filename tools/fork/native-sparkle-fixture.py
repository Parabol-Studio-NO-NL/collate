#!/usr/bin/env python3
"""Serve and exercise the isolated native Sparkle integration fixture."""

import argparse
import ctypes
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import plistlib
import shutil
import signal
import socket
from socketserver import TCPServer
import ssl
import subprocess
import sys
import time


UPDATE_PATHS = {
    "/updates/stable/appcast.xml": "application/rss+xml",
    "/updates/stable/releases/codesign-v90.1.0/Collate.zip": "application/zip",
}


def serve(docroot, cert, key, port_file, request_log):
    root = docroot.resolve(strict=True)

    class UpdateHandler(BaseHTTPRequestHandler):
        def do_HEAD(self):
            self.respond(send_body=False)

        def do_GET(self):
            self.respond(send_body=True)

        def respond(self, send_body):
            content_type = UPDATE_PATHS.get(self.path)
            if content_type is None:
                self.send_error(404)
                return
            path = root / self.path.lstrip("/")
            try:
                if path.resolve(strict=True) != path or not path.is_file():
                    self.send_error(404)
                    return
                source = path.open("rb")
            except OSError:
                self.send_error(404)
                return
            with source:
                self.send_response(200)
                self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(os.fstat(source.fileno()).st_size))
                self.send_header("Cache-Control", "no-store")
                self.end_headers()
                if send_body:
                    shutil.copyfileobj(source, self.wfile)
                    with request_log.open("a", encoding="utf-8") as log:
                        log.write(self.path + "\n")

        def log_message(self, _format, *_args):
            pass

    class LoopbackServer(ThreadingHTTPServer):
        def server_bind(self):
            # HTTPServer resolves the address to derive its name. This server only accepts localhost.
            TCPServer.server_bind(self)
            self.server_name = "localhost"
            self.server_port = self.server_address[1]

    context = ssl.create_default_context(ssl.Purpose.CLIENT_AUTH)
    context.load_cert_chain(certfile=cert, keyfile=key)
    request_log.write_text("", encoding="utf-8")
    with LoopbackServer(("127.0.0.1", 0), UpdateHandler) as server:
        server.socket = context.wrap_socket(server.socket, server_side=True)
        port_file.write_text(f"{server.server_port}\n", encoding="utf-8")
        server.serve_forever()


class ScratchProcesses:
    def __init__(self, binary):
        self.binary = binary
        self.libproc = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
        self.libproc.proc_pidpath.argtypes = (ctypes.c_int, ctypes.c_void_p, ctypes.c_uint32)
        self.libproc.proc_pidpath.restype = ctypes.c_int

    def matches(self, pid):
        path = ctypes.create_string_buffer(4096)
        size = self.libproc.proc_pidpath(pid, path, len(path))
        return size > 0 and os.path.realpath(os.fsdecode(path.value)) == str(self.binary)

    def pids(self):
        output = subprocess.check_output(["/bin/ps", "-axo", "pid="], text=True, timeout=5)
        pids = [int(value) for value in output.split()]
        return [pid for pid in pids if self.matches(pid)]

    def stop(self, pid, signum):
        if self.matches(pid):
            try:
                os.kill(pid, signum)
            except ProcessLookupError:
                pass

    def cleanup(self):
        for pid in self.pids():
            self.stop(pid, signal.SIGTERM)
        deadline = time.monotonic() + 5
        while pids := self.pids():
            if time.monotonic() >= deadline:
                for pid in pids:
                    self.stop(pid, signal.SIGKILL)
                return
            time.sleep(0.2)


def read_log(path):
    if not path.exists():
        return ""
    return path.read_text(encoding="utf-8", errors="replace")


def rpc(port, method, params=None):
    request = json.dumps({"id": 1, "method": method, "params": params or {}}) + "\n"
    with socket.create_connection(("127.0.0.1", port), timeout=3) as connection:
        connection.sendall(request.encode("utf-8"))
        with connection.makefile("rb") as response:
            return json.loads(response.readline())


def build_number(app):
    with (app / "Contents/Info.plist").open("rb") as info:
        return plistlib.load(info)["CFBundleVersion"]


def wait_for_control(child, port, app_log, click_log, deadline):
    listening = f"control server listening on 127.0.0.1:{port}"
    while time.monotonic() < deadline:
        if (status := child.poll()) is not None:
            raise RuntimeError(f"Collate exited before the control request with status {status}")
        if listening in read_log(app_log) and "observer-ready" in read_log(click_log).splitlines():
            try:
                if rpc(port, "ui.inspect").get("ok") is True:
                    return
            except (OSError, ValueError):
                pass
        time.sleep(0.2)
    raise RuntimeError("The scratch app and native update observer did not become ready")


def wait_for_relaunch(child, processes, deadline):
    while time.monotonic() < deadline:
        if (status := child.poll()) is not None:
            if status != 0:
                raise RuntimeError(f"The older Collate process exited abnormally with status {status}")
            for pid in processes.pids():
                if pid != child.pid:
                    return pid
        time.sleep(0.25)
    raise RuntimeError("Sparkle did not exit the old process and relaunch the scratch app")


def exercise(app_path, workdir, click_dylib):
    if sys.platform != "darwin":
        raise RuntimeError("The native Sparkle fixture requires macOS")
    work = workdir.resolve(strict=True)
    runner_temp = Path(os.environ["RUNNER_TEMP"]).resolve(strict=True)
    app = app_path.resolve(strict=True)
    dylib = click_dylib.resolve(strict=True)
    binary = app / "Contents/MacOS/Collate"
    if work == runner_temp or not work.is_relative_to(runner_temp):
        raise RuntimeError("Use a fresh fixture directory under RUNNER_TEMP")
    if app.name != "Collate.app" or not app.is_relative_to(work) or binary.resolve(strict=True) != binary:
        raise RuntimeError("The Collate bundle and executable must be inside the fixture directory")
    if not dylib.is_relative_to(work) or not dylib.is_file():
        raise RuntimeError("The native observer library must be inside the fixture directory")
    if build_number(app) != "1":
        raise RuntimeError("Expected the fixture to start at build 1")

    processes = ScratchProcesses(binary)
    if processes.pids():
        raise RuntimeError("The scratch executable is already running")
    app_log = work / "app.log"
    click_log = work / "click.log"
    request_log = work / "requests.log"
    click_log.write_text("", encoding="utf-8")
    environment = {name: value for name, value in os.environ.items() if not name.startswith("DYLD_")}
    environment["DESIGNCRAFT_CONFIG_DIR"] = str(work / "profile")
    child_environment = {
        **environment,
        "DYLD_INSERT_LIBRARIES": str(dylib),
        "COLLATE_SPARKLE_CLICK_LOG": str(click_log),
    }
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]

    try:
        with app_log.open("wb") as log:
            child = subprocess.Popen([binary, "--control", str(port)], stdout=log, stderr=subprocess.STDOUT, env=child_environment)
            try:
                deadline = time.monotonic() + 150
                wait_for_control(child, port, app_log, click_log, min(deadline, time.monotonic() + 30))
                request_offset = len(read_log(request_log).splitlines())
                result = rpc(port, "ui.menu.invoke", {"command": "help.checkForUpdates", "params": {}})
                if result.get("ok") is not True:
                    raise RuntimeError(f"Check for Updates command failed: {result}")

                relaunch_pid = wait_for_relaunch(child, processes, deadline)
                if build_number(app) != "2":
                    raise RuntimeError("Sparkle relaunched without installing fixture build 2")
                subprocess.run(["/usr/bin/codesign", "--verify", "--strict", "--deep", app], check=True, timeout=15)
                version = subprocess.check_output([binary, "--version"], text=True, env=environment, timeout=15).strip()
                if version != "Collate 90.1.0":
                    raise RuntimeError(f"Unexpected installed binary version: {version}")
                requests = read_log(request_log).splitlines()[request_offset:]
                if not UPDATE_PATHS.keys() <= set(requests):
                    raise RuntimeError("Sparkle did not fetch both the appcast and the archive")
                click_events = read_log(click_log).splitlines()
                ready = click_events.index("observer-ready")
                if not any(event.startswith("clicked ") for event in click_events[ready + 1:]):
                    raise RuntimeError("The observer did not click the native Sparkle update dialog")
                if not processes.matches(relaunch_pid):
                    raise RuntimeError("The relaunched fixture exited unexpectedly")
                print(f"Collate checked for updates, installed build 2 and relaunched from PID {child.pid} as PID {relaunch_pid}.")
            finally:
                try:
                    if child.poll() is None:
                        child.terminate()
                        try:
                            child.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            child.kill()
                            child.wait(timeout=5)
                finally:
                    processes.cleanup()
    except Exception:
        for path in (app_log, click_log, request_log):
            print(f"--- {path.name} ---", file=sys.stderr)
            print(read_log(path)[-12000:], file=sys.stderr)
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    server = commands.add_parser("serve")
    for name in ("docroot", "cert", "key", "port_file", "request_log"):
        server.add_argument(name, type=Path)
    app = commands.add_parser("exercise")
    for name in ("app", "workdir", "click_dylib"):
        app.add_argument(name, type=Path)
    args = parser.parse_args()
    if args.command == "serve":
        serve(args.docroot, args.cert, args.key, args.port_file, args.request_log)
    else:
        exercise(args.app, args.workdir, args.click_dylib)


if __name__ == "__main__":
    main()
