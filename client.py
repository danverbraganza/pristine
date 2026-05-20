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


HELP_TEXT = """\
Client commands (handled locally, not sent to the agent):
  /help   show this message
  /quit   exit the client (Ctrl+D / Ctrl+C also work)
Any other input is forwarded to the agent as a user message.\
"""


class RpcError(Exception):
    """JSON-RPC error response returned by the server."""

    def __init__(self, method: str, code: int | None, message: str) -> None:
        super().__init__(f"{method} -> [{code}] {message}")
        self.method = method
        self.code = code
        self.message = message


def send(proc: subprocess.Popen, method: str, params: dict | None = None) -> dict:
    """Send a JSON-RPC request and return its `result` payload.

    Raises RpcError if the server returned an `error` response.
    """
    req = request_json(method, params=params)
    assert proc.stdin is not None
    proc.stdin.write(req + "\n")
    proc.stdin.flush()
    msg = read_response(proc)
    if "error" in msg:
        err = msg["error"]
        raise RpcError(method, err.get("code"), err.get("message", ""))
    return msg.get("result", {})


def _parse_message(line: str) -> dict | None:
    """Parse one JSON-RPC line from the server.

    Returns None for malformed lines (logged to stderr) so the read
    loops can skip them rather than crashing the client.
    """
    try:
        return json.loads(line)
    except json.JSONDecodeError:
        print(f"[server stdout (non-JSON)]: {line.rstrip()}", file=sys.stderr)
        return None


def read_response(proc: subprocess.Popen) -> dict:
    """Read lines until we get a JSON-RPC response (has an 'id' field)."""
    assert proc.stdout is not None
    while True:
        line = proc.stdout.readline()
        if not line:
            print("Server closed stdout unexpectedly", file=sys.stderr)
            sys.exit(1)
        msg = _parse_message(line)
        if msg is None:
            continue
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
        msg = _parse_message(line)
        if msg is None:
            continue
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
        try:
            result = send(proc, "initialize")
        except RpcError as e:
            print(f"Failed to initialize: {e}", file=sys.stderr)
            sys.exit(1)
        agent_id = result["agent_id"]
        print(f"Pristine ({agent_id[:8]}) -- type /help for client commands", file=sys.stderr)
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
            stripped = user_input.strip()
            if not stripped:
                continue
            if stripped == "/quit":
                break
            if stripped == "/help":
                print(HELP_TEXT, file=sys.stderr)
                continue
            try:
                send(proc, "send_message", {"agent_id": agent_id, "content": user_input})
            except RpcError as e:
                print(f"send_message failed: {e}", file=sys.stderr)
                continue
            drain_events(proc)
    finally:
        if proc.poll() is None:
            try:
                send(proc, "shutdown")
            except RpcError as e:
                print(f"shutdown error: {e}", file=sys.stderr)
        proc.stdin.close()
        proc.wait()


if __name__ == "__main__":
    main()
