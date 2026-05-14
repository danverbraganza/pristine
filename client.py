# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "jsonrpcclient",
# ]
# ///

"""JSON-RPC client for the Pristine agent harness.

Spawns `cargo run -- run` as a subprocess and drives a two-message conversation
over the stdio JSON-RPC transport.  Runnable as `uv run client.py`.
"""

import json
import subprocess
import sys

_next_id = 0


def make_request(method: str, params: dict | None = None) -> str:
    global _next_id
    _next_id += 1
    r: dict = {"jsonrpc": "2.0", "method": method, "id": _next_id}
    if params is not None:
        r["params"] = params
    return json.dumps(r)


def send(proc: subprocess.Popen, method: str, params: dict | None = None) -> dict:
    """Send a JSON-RPC request and return the parsed response."""
    req = make_request(method, params)
    print(f"-> {method}", file=sys.stderr)
    assert proc.stdin is not None
    proc.stdin.write(req + "\n")
    proc.stdin.flush()
    return read_response(proc)


def read_response(proc: subprocess.Popen) -> dict:
    """Read lines until we get a JSON-RPC response (has an 'id' field)."""
    assert proc.stdout is not None
    while True:
        line = proc.stdout.readline()
        if not line:
            print("Server closed stdout unexpectedly", file=sys.stderr)
            sys.exit(1)
        msg = json.loads(line)
        if "id" in msg:
            return msg


def drain_events(proc: subprocess.Popen) -> None:
    """Read agent.event notifications, printing token deltas until run_complete."""
    assert proc.stdout is not None
    while True:
        line = proc.stdout.readline()
        if not line:
            print("\nServer closed stdout unexpectedly", file=sys.stderr)
            sys.exit(1)
        msg = json.loads(line)
        if "id" in msg:
            # Unexpected response; skip it.
            continue
        params = msg.get("params", {})
        event_type = params.get("type", "")
        if event_type == "token_delta":
            text = params.get("data", {}).get("text", "")
            print(text, end="", flush=True)
        elif event_type == "error":
            error_msg = params.get("data", {}).get("message", "unknown error")
            print(f"\nAgent error: {error_msg}", file=sys.stderr)
            break
        elif event_type == "run_complete":
            print()  # newline after streamed tokens
            break


def main() -> None:
    proc = subprocess.Popen(
        ["cargo", "run", "--", "run"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=None,  # inherit, goes to terminal
        text=True,
        bufsize=1,
    )

    try:
        # Handshake
        resp = send(proc, "initialize")
        result = resp["result"]
        agent_id = result["agent_id"]
        owner_id = result["owner_id"]
        print(f"agent_id: {agent_id}", file=sys.stderr)
        print(f"owner_id: {owner_id}", file=sys.stderr)

        # First message
        send(proc, "send_message", {"agent_id": agent_id, "content": "Introduce yourself to me, Pristine"})
        drain_events(proc)

        # Second message
        send(proc, "send_message", {"agent_id": agent_id, "content": "Write me a poem of what it is like to be you, Pristine"})
        drain_events(proc)

        # Shutdown
        send(proc, "shutdown")
    finally:
        proc.stdin.close()
        proc.wait()


if __name__ == "__main__":
    main()
