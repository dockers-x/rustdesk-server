#!/usr/bin/env python3
"""Exercise the HTTP diagnostics of actual HBBS and HBBR processes."""

import contextlib
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time


BINARIES = Path(sys.argv[1]).resolve()
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
                wait_until(lambda: release_fifo(fifo), "HBBR did not open the startup FIFO")
                wait_until(lambda: status(hbbr_health, "/readyz") == 200, "HBBR never became ready")
                verify_update_proxy(proxy, False)

            verify_json(hbbr_data / "hbbr.log", "hbbr", ["health.listening", "service.ready"])
            assert status(hbbr_health, "/livez") is None
            fifo.unlink()

            with daemon("hbbr", hbbr_data, hbbr_port, hbbr_env):
                wait_until(lambda: status(hbbr_health, "/readyz") == 200, "HBBR did not recover")

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

    print("Daemon HTTP diagnostics, logging, and update opt-out passed")


if __name__ == "__main__":
    main()
