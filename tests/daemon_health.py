#!/usr/bin/env python3
"""Run the S6 health script against actual HBBS and HBBR processes."""

import contextlib
import http.client
import http.server
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time


ROOT = Path(__file__).resolve().parent.parent
BINARIES = Path(sys.argv[1]).resolve()
CONSOLE_BINARY = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else None
KEY = "daemon-health-test-credential"


def reserve_ports(offsets, udp=False):
    for _ in range(100):
        with socket.socket() as primary:
            primary.bind(("127.0.0.1", 0))
            port = primary.getsockname()[1]
            if not 3 <= port <= 65533:
                continue
            try:
                with contextlib.ExitStack() as extras:
                    for offset in offsets:
                        reserved = extras.enter_context(socket.socket())
                        reserved.bind(("127.0.0.1", port + offset))
                    if udp:
                        reserved = extras.enter_context(socket.socket(type=socket.SOCK_DGRAM))
                        reserved.bind(("127.0.0.1", port))
                    return port
            except OSError:
                continue
    raise RuntimeError("No free port group for RustDesk listeners")


def status(port, route):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
    try:
        connection.request("GET", route)
        response = connection.getresponse()
        response.read()
        return response.status
    except (OSError, http.client.HTTPException):
        return None
    finally:
        connection.close()


def wait_until(predicate, description):
    deadline = time.monotonic() + 10
    while not predicate():
        assert time.monotonic() < deadline, description
        time.sleep(0.05)


@contextlib.contextmanager
def daemon(name, cwd, port, extra_env):
    params = [BINARIES / name, "-b", "127.0.0.1", "-p", str(port), "-k", KEY]
    with (cwd / f"{name}.log").open("w") as stdout:
        env = os.environ | {"TEST_HBBS": "no", "RUST_LOG": "debug"} | extra_env
        process = subprocess.Popen(params, cwd=cwd, env=env, stdout=stdout, stderr=stdout)
        try:
            yield
        finally:
            stop_process(process)


def stop_process(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


@contextlib.contextmanager
def console(cwd):
    if CONSOLE_BINARY:
        port = reserve_ports(())
        cwd.mkdir()
        config = cwd / "config.yaml"
        config.write_text(f'gin:\n  api-addr: "127.0.0.1:{port}"\n')
        with (cwd / "console.log").open("w") as stdout:
            process = subprocess.Popen([CONSOLE_BINARY, "-c", config], cwd=cwd, stdout=stdout, stderr=stdout)
            try:
                wait_until(lambda: status(port, "/health") == 200, "Console never became healthy")
                yield port
            finally:
                stop_process(process)
        return

    class HealthHandler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            self.send_response(200 if self.path == "/health" else 404)
            self.end_headers()

        def log_message(self, *_args):
            pass

    with http.server.HTTPServer(("127.0.0.1", 0), HealthHandler) as server:
        thread = threading.Thread(target=server.serve_forever)
        thread.start()
        try:
            yield server.server_port
        finally:
            server.shutdown()
            thread.join()


def healthcheck(env, component=None, detail=None):
    result = subprocess.run(
        ["sh", ROOT / "docker/rootfs/usr/bin/healthcheck.sh"],
        env=os.environ | env,
        capture_output=True,
        text=True,
        timeout=5,
    )
    assert result.returncode == (1 if component else 0), result.stderr
    if component:
        assert component in result.stderr, result.stderr
    if detail:
        assert detail in result.stderr, result.stderr


def verify_json(path, component, events):
    lines = path.read_text().splitlines()
    records = [json.loads(line) for line in lines]
    assert records and all(
        record["timestamp"].endswith("Z") and record["level"] and record["component"] == component
        for record in records
    ), records
    assert set(events) <= {record["event"] for record in records}, records
    assert KEY not in "\n".join(lines)
    return records


def release_fifo(path):
    try:
        writer = os.open(path, os.O_WRONLY | os.O_NONBLOCK)
    except OSError:
        return False
    os.close(writer)
    return True


def verify_update_proxy(proxy, expect_traffic):
    proxy.settimeout(5 if expect_traffic else 0.2)
    try:
        connection, _ = proxy.accept()
    except TimeoutError:
        assert not expect_traffic, "HBBS did not request an official update with checks enabled"
        return
    with connection:
        assert expect_traffic, "HBBS contacted the update proxy with checks disabled"
        connection.settimeout(2)
        with connection.makefile("rb") as stream:
            assert stream.readline().startswith(b"CONNECT api.rustdesk.com:443 "), "Unexpected outbound update request"


def main():
    with tempfile.TemporaryDirectory(prefix="rustdesk-daemon-health-") as tmp, socket.socket() as proxy:
        workdir = Path(tmp)
        hbbs_data = workdir / "hbbs"
        hbbr_data = workdir / "hbbr"
        hbbs_data.mkdir()
        hbbr_data.mkdir()
        fifo = hbbr_data / "blacklist.txt"
        os.mkfifo(fifo)

        hbbs_port = reserve_ports((-1, 2), udp=True)
        hbbr_port = reserve_ports((2,))
        hbbs_health = reserve_ports(())
        hbbr_health = reserve_ports(())
        # Pin test probes to this local proxy and observe whether the official request is attempted.
        proxy.bind(("127.0.0.1", 0))
        proxy.listen()
        proxy_url = f"http://127.0.0.1:{proxy.getsockname()[1]}"
        hbbs_env = {
            "HBBS_HEALTH_PORT": str(hbbs_health),
            "DISABLE_UPDATE_CHECK": "Y",
            "LOG_FORMAT": "json",
            "HTTPS_PROXY": proxy_url,
            "https_proxy": proxy_url,
            "NO_PROXY": "",
            "no_proxy": "",
        }
        hbbr_env = {"HBBR_HEALTH_PORT": str(hbbr_health), "LOG_FORMAT": "json"}
        check_env = hbbs_env | hbbr_env | {"CONSOLE_ENABLED": "0"}
        with daemon("hbbs", hbbs_data, hbbs_port, hbbs_env):
            wait_until(lambda: status(hbbs_health, "/readyz") == 200, "HBBS never became ready")
            assert status(hbbs_health, "/livez") == 200
            assert status(hbbs_health, "/unknown") == 404
            # A specific second loopback address can bind only if the probe is not wildcard-bound.
            with socket.socket() as alternate:
                alternate.bind(("127.0.0.2", hbbs_health))

            with daemon("hbbr", hbbr_data, hbbr_port, hbbr_env):
                wait_until(lambda: status(hbbr_health, "/livez") == 200, "HBBR never became live")
                assert status(hbbr_health, "/readyz") == 503
                healthcheck(check_env, "HBBR", "HTTP 503")
                wait_until(lambda: release_fifo(fifo), "HBBR did not open the startup FIFO")
                wait_until(lambda: status(hbbr_health, "/readyz") == 200, "HBBR never became ready")
                healthcheck(check_env)
                with console(workdir / "console") as port:
                    console_env = check_env | {
                        "CONSOLE_ENABLED": "1",
                        "CONSOLE_HEALTH_PORT": "",
                        "RUSTDESK_API_GIN_API_ADDR": f"127.0.0.1:{port}",
                    }
                    healthcheck(console_env)
                    healthcheck(console_env | {"RUSTDESK_API_GIN_API_ADDR": "", "CONSOLE_HEALTH_PORT": str(port)})
                healthcheck(console_env, "Console")
                verify_update_proxy(proxy, False)

            verify_json(hbbr_data / "hbbr.log", "hbbr", ["health.listening", "service.ready"])
            assert status(hbbr_health, "/livez") is None
            healthcheck(check_env, "HBBR")
            fifo.unlink()

            with daemon("hbbr", hbbr_data, hbbr_port, hbbr_env):
                wait_until(lambda: status(hbbr_health, "/readyz") == 200, "HBBR did not recover")
                healthcheck(check_env)

        records = verify_json(hbbs_data / "hbbs.log", "hbbs", ["health.listening", "service.ready", "update.disabled"])
        assert all(record["event"] != "update.check" for record in records)
        with daemon("hbbs", hbbs_data, hbbs_port, hbbs_env | {"DISABLE_UPDATE_CHECK": "N"}):
            verify_update_proxy(proxy, True)
        verify_json(hbbs_data / "hbbs.log", "hbbs", ["update.check"])

        with daemon("hbbr", hbbr_data, hbbr_port, {"LOG_FORMAT": "", "HBBR_HEALTH_PORT": "0"}):
            def accepts_relay_tcp():
                try:
                    with socket.create_connection(("127.0.0.1", hbbr_port), timeout=0.2):
                        return True
                except OSError:
                    return False

            wait_until(accepts_relay_tcp, "HBBR standalone listener did not start")
            assert status(hbbr_health, "/livez") is None
        assert "Listening on tcp" in (hbbr_data / "hbbr.log").read_text()

    print("Daemon health, S6 aggregate, logging, and update opt-out passed")


if __name__ == "__main__":
    main()
