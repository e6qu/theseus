#!/usr/bin/env python3
"""API contract fixture, not a VM or a determinism demonstration."""

import hashlib
import json
from pathlib import Path
import socket
import sys

MODE = "exit"
ENTROPY_DEVICE = True
entropy_configured = False
sock = sys.argv[sys.argv.index("--api-sock") + 1]
server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(sock)
server.listen()
serial = None
output = None
expected = None
trace = []
error = None
started = False


def ledger(records):
    digest = hashlib.sha256()
    for record in records:
        data = record.encode()
        digest.update(len(data).to_bytes(8, "little"))
        digest.update(data)
    return {"decisions": len(records), "sha256": digest.hexdigest(), "tail": records[-32:]}


def admit(record):
    global error
    if expected is not None and expected[len(trace):len(trace) + 1] != [record]:
        error = f"machine execution replay diverged at decision {len(trace)}: observed {record}"
        return False
    trace.append(record)
    return True


def evidence(boundary):
    replay_error = error
    if replay_error is None and expected is not None and trace != expected:
        replay_error = f"machine execution replay stopped at decision {len(trace)} of {len(expected)}"
    local = [record.split(":", 2)[2] for record in trace if record.startswith("vcpu:0:")]
    output.write_text(json.dumps({
        "format": "theseus-execution-v1", "boundary": boundary,
        "execution_ledgers": [ledger(local)],
        "machine_execution_ledger": ledger(trace), "machine_execution_trace": trace,
        "replay_error": replay_error,
    }) + "\n")


while True:
    connection, _ = server.accept()
    with connection:
        request = bytearray()
        while b"\r\n\r\n" not in request:
            chunk = connection.recv(4096)
            if not chunk:
                break
            request.extend(chunk)
        if not request:
            continue
        header, body = bytes(request).split(b"\r\n\r\n", 1)
        length = next(int(line.split(b":", 1)[1]) for line in header.split(b"\r\n")
                      if line.lower().startswith(b"content-length:"))
        while len(body) < length:
            chunk = connection.recv(length - len(body))
            assert chunk, "incomplete request body"
            body += chunk
        method, endpoint, _ = header.split(b"\r\n", 1)[0].decode().split()
        body = json.loads(body)
        done = False
        status = "204 No Content"
        if endpoint == "/boot-source":
            assert body["boot_args"] == "console=ttyS0 reboot=k panic=-1 quiet loglevel=0"
        elif endpoint == "/serial":
            serial = Path(body["serial_out_path"])
        elif endpoint == "/execution" and method == "PUT":
            output = Path(body["evidence_path"])
            assert not output.exists(), "capture must not overwrite an existing file"
            output.touch()
            if body["replay_trace_path"]:
                expected = json.loads(Path(body["replay_trace_path"]).read_text())
        elif endpoint == "/entropy":
            assert ENTROPY_DEVICE, "unused RNG must not be attached"
            entropy_configured = True
        elif endpoint == "/actions":
            assert output is not None, "execution must be configured before boot"
            assert entropy_configured == ENTROPY_DEVICE
            started = True
            serial.write_text("THES:M:42\n")
        elif endpoint == "/serial-input":
            assert started
            data = bytes.fromhex(body["data_hex"])
            if admit(f"host:serial_input:{len(data)}:{data.hex()}"):
                serial.write_text(serial.read_text() + "sensor reading: " + data.decode())
                if MODE == "exit":
                    admit("vcpu:0:pio_write:0x64:1:fe")
                    evidence("guest_exit" if error is None else "runtime_error")
                    done = True
            else:
                status = "400 Bad Request"
                evidence("runtime_error")
                done = True
        elif endpoint == "/vm":
            assert method == "PATCH" and body == {"state": "Paused"}
        elif endpoint == "/execution" and method == "PATCH":
            evidence("pause")
        connection.sendall(f"HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n".encode())
    if done:
        sys.exit(1 if error else 0)
