# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "jsonrpcclient",
#     "rich",
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
from rich.console import Console

# Rich console for highlighted event output; auto-disables color when
# stderr is not a TTY (e.g. piped to a file).
err_console = Console(stderr=True, highlight=False)


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


def _truncate(s: str, n: int) -> str:
    """Shorten `s` to at most `n` visible chars, appending an ellipsis."""
    return s if len(s) <= n else s[: n - 1] + "\u2026"


REGISTRY: dict[str, type["Tool"]] = {}


class Tool:
    """Rendering logic for one tool call/result pair.

    Subclasses declare their name as a class attribute and override only the
    methods whose default behaviour doesn't fit.  The registry is populated
    automatically by __init_subclass__, so adding a new tool is just writing
    a new class -- no dispatch table to maintain by hand.
    """

    name: str = ""  # overridden in every concrete subclass

    def __init_subclass__(cls, **kwargs: object) -> None:
        super().__init_subclass__(**kwargs)
        if cls.name:
            REGISTRY[cls.name] = cls

    @classmethod
    def from_name(cls, name: str) -> "Tool":
        """Return the right Tool instance for *name*, or a generic fallback."""
        return REGISTRY.get(name, UnknownTool)(name)

    def __init__(self, name: str) -> None:
        self._name = name  # runtime name (matters for UnknownTool)

    def call_signature(self, args: dict) -> str:
        """One-line human-readable call signature."""
        return f"{self._name}({_truncate(json.dumps(args), 100)})"

    def result_summary(self, result: object, is_error: bool) -> str:
        """Short outcome string that follows `->` on the result line."""
        if is_error:
            msg = result.get("error") if isinstance(result, dict) else None  # type: ignore[union-attr]
            return f"error: {msg or json.dumps(result)}"
        return _truncate(json.dumps(result), 80)

    def print_result_body(self, result: object, is_error: bool) -> None:
        """Emit long-form body content (e.g. file text, stdout).

        Most tools produce nothing here; only override when there is
        human-consumable multi-line output worth showing.
        """


class UnknownTool(Tool):
    """Catch-all for tool names not in the registry; uses all base defaults."""

    name = ""  # deliberately blank -- not registered


class ReadTool(Tool):
    name = "read"

    def call_signature(self, args: dict) -> str:
        path = args.get("path", "?")
        start = args.get("start_line")
        end = args.get("end_line")
        if start is not None or end is not None:
            return f'read("{path}", lines {start or 1}-{end if end is not None else "end"})'
        return f'read("{path}")'

    def result_summary(self, result: object, is_error: bool) -> str:
        if is_error or not isinstance(result, dict):
            return super().result_summary(result, is_error)
        content = result.get("content", "")
        if not content:
            n_lines = 0
        elif content.endswith("\n"):
            n_lines = content.count("\n")
        else:
            n_lines = content.count("\n") + 1
        return f"success, {n_lines} line{'' if n_lines == 1 else 's'} read"

    def print_result_body(self, result: object, is_error: bool) -> None:
        if is_error or not isinstance(result, dict):
            return
        content = result.get("content", "")
        if content:
            err_console.out(content, end="" if content.endswith("\n") else "\n")


class WriteTool(Tool):
    name = "write"

    def call_signature(self, args: dict) -> str:
        path = args.get("path", "?")
        size = len(args.get("content", "").encode("utf-8"))
        return f'write("{path}", {size} bytes)'

    def result_summary(self, result: object, is_error: bool) -> str:
        if is_error or not isinstance(result, dict):
            return super().result_summary(result, is_error)
        n_bytes = result.get("bytes_written", "?")
        return f"success, {n_bytes} byte{'' if n_bytes == 1 else 's'} written"


class EditTool(Tool):
    name = "edit"

    def call_signature(self, args: dict) -> str:
        return f'edit("{args.get("path", "?")}")'

    def result_summary(self, result: object, is_error: bool) -> str:
        if is_error or not isinstance(result, dict):
            return super().result_summary(result, is_error)
        return "success" if "bytes_written" in result else "unchanged"


class InsertTool(Tool):
    name = "insert"

    def call_signature(self, args: dict) -> str:
        return f'insert("{args.get("path", "?")}", after line {args.get("after_line", "?")})'

    def result_summary(self, result: object, is_error: bool) -> str:
        if is_error or not isinstance(result, dict):
            return super().result_summary(result, is_error)
        n = result.get("lines_inserted", 0)
        return f"success, {n} line{'' if n == 1 else 's'} inserted"


class ExecBashTool(Tool):
    name = "exec_bash"

    def call_signature(self, args: dict) -> str:
        cmd = _truncate(args.get("command", ""), 80)
        return f"exec_bash({json.dumps(cmd)})"

    def result_summary(self, result: object, is_error: bool) -> str:
        if is_error or not isinstance(result, dict):
            return super().result_summary(result, is_error)
        status = result.get("status", {})
        parts: list[str] = []
        if isinstance(status, dict):
            kind = status.get("status")
            if kind == "exit":
                parts.append(f"exit {status.get('code', '?')}")
            elif kind == "signal":
                parts.append(f"signal {status.get('name', '?')}")
            elif kind == "timeout":
                parts.append("timeout")
            else:
                parts.append(json.dumps(status))
        else:
            parts.append(str(status))
        if result.get("stdout_truncated"):
            parts.append("stdout truncated")
        if result.get("stderr_truncated"):
            parts.append("stderr truncated")
        return ", ".join(parts)

    def print_result_body(self, result: object, is_error: bool) -> None:
        if is_error or not isinstance(result, dict):
            return
        stdout = result.get("stdout", "")
        stderr = result.get("stderr", "")
        if stdout:
            err_console.print("[dim]--- stdout ---[/]")
            err_console.out(stdout, end="" if stdout.endswith("\n") else "\n")
        if stderr:
            err_console.print("[dim]--- stderr ---[/]")
            err_console.out(stderr, end="" if stderr.endswith("\n") else "\n")


class AddTool(Tool):
    name = "add"

    def call_signature(self, args: dict) -> str:
        return f"add({args.get('a', '?')}, {args.get('b', '?')})"

    def result_summary(self, result: object, is_error: bool) -> str:
        if is_error or not isinstance(result, dict):
            return super().result_summary(result, is_error)
        return f"sum={result.get('sum', '?')}"


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


def drain_events(proc: subprocess.Popen, pending_calls: dict[str, dict]) -> None:
    """Read agent.event notifications until 'idle' arrives.

    `pending_calls` maps tool_use_id -> the call's arguments dict, so a
    tool_result event can replay the same call signature on its summary line.
    """
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
            print(f"[drain_events] unexpected response message (id={msg['id']}); ignoring", file=sys.stderr)
            continue
        params = msg.get("params", {})
        event_type = params.get("type", "")
        agent_short = str(params.get("agent_id", ""))[:8]
        if event_type == "token_delta":
            text = params.get("data", {}).get("text", "")
            print(text, end="", flush=True)
        elif event_type == "block_complete":
            data = params.get("data", {})
            block_type = data.get("block_type")
            if block_type == "tool_call":
                name = data.get("name", "?")
                args = data.get("arguments", {}) or {}
                tool_use_id = data.get("id")
                if tool_use_id:
                    pending_calls[tool_use_id] = args
                tool = Tool.from_name(name)
                err_console.print(
                    f"\n[bold cyan]>>>[/] [bold]Agent #{agent_short}[/]: {tool.call_signature(args)}"
                )
            elif block_type == "tool_result":
                name = data.get("name", "?")
                result = data.get("result")
                is_error = bool(data.get("is_error", False))
                tool_use_id = data.get("tool_use_id")
                args = pending_calls.pop(tool_use_id, {}) if tool_use_id else {}
                tool = Tool.from_name(name)
                marker_color = "red" if is_error else "green"
                err_console.print(
                    f"[bold {marker_color}]>>>[/] [bold]Agent #{agent_short}[/]: "
                    f"{tool.call_signature(args)} -> {tool.result_summary(result, is_error)}"
                )
                tool.print_result_body(result, is_error)
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

        pending_calls: dict[str, dict] = {}
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
            drain_events(proc, pending_calls)
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
