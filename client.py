# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "jsonrpcclient",
# ]
# ///

"""Chat REPL for the Pristine agent harness.

Spawns `cargo run -- run` as a subprocess and drives one user turn per input
line.  When stdin is a TTY this runs as an interactive prompt; when stdin is
not a TTY (e.g. piped from a fixture or heredoc) it runs in scripted mode --
each input line is echoed to stderr for transcript clarity and the client
exits cleanly on EOF.  Runnable as `uv run client.py`.
"""

import json
import subprocess
import sys

from jsonrpcclient import request_json


def send(proc: subprocess.Popen, method: str, params: dict | None = None) -> dict:
    """Send a JSON-RPC request and return the parsed response."""
    req = request_json(method, params=params)
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
    """Read agent.event notifications until 'idle' arrives."""
    assert proc.stdout is not None
    while True:
        line = proc.stdout.readline()
        if not line:
            print("\nServer closed stdout unexpectedly", file=sys.stderr)
            sys.exit(1)
        msg = json.loads(line)
        if "id" in msg:
            continue
        params = msg.get("params", {})
        event_type = params.get("type", "")
        if event_type == "token_delta":
            text = params.get("data", {}).get("text", "")
            print(text, end="", flush=True)
        elif event_type == "block_complete":
            data = params.get("data", {})
            block_type = data.get("block_type")
            if block_type == "tool_call":
                name = data.get("name", "?")
                args = data.get("arguments", {})
                print(f"\n[tool call: {name}({json.dumps(args)})]", file=sys.stderr)
            elif block_type == "tool_result":
                name = data.get("name", "?")
                result = data.get("result")
                is_error = data.get("is_error", False)
                marker = "tool error" if is_error else "tool result"
                print(f"[{marker}: {name} -> {json.dumps(result)}]", file=sys.stderr)
            # user_message / agent_message / reasoning_trace are observation-only
        elif event_type == "error":
            error_msg = params.get("data", {}).get("message", "unknown error")
            print(f"\nAgent error: {error_msg}", file=sys.stderr)
            break
        elif event_type == "idle":
            print()  # final newline after streamed tokens / tool activity
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
        resp = send(proc, "initialize")
        result = resp["result"]
        agent_id = result["agent_id"]
        print(f"Pristine ({agent_id[:8]})", file=sys.stderr)
        scripted = not sys.stdin.isatty()
        if scripted:
            print("(scripted mode: reading turns from stdin)", file=sys.stderr)

        while True:
            try:
                if scripted:
                    line = sys.stdin.readline()
                    if not line:
                        raise EOFError
                    user_input = line.rstrip("\n")
                    if user_input.strip():
                        print(f"> {user_input}", file=sys.stderr, flush=True)
                else:
                    user_input = input("> ")
            except (EOFError, KeyboardInterrupt):
                print(file=sys.stderr)
                break
            if not user_input.strip():
                continue
            send(proc, "send_message", {"agent_id": agent_id, "content": user_input})
            drain_events(proc)
    finally:
        if proc.poll() is None:
            send(proc, "shutdown")
        proc.stdin.close()
        proc.wait()


if __name__ == "__main__":
    main()
