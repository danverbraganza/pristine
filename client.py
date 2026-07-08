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

import argparse
import json
import pathlib
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


def _safe_json(obj: object) -> str:
    """JSON-encode `obj`, falling back to `repr` for non-serializable values."""
    try:
        return json.dumps(obj)
    except (TypeError, ValueError):
        return repr(obj)


# Box-drawing gutter prefixing every line of a forked peer's output, so its
# activity is visually distinct from the messaged agent's.
FORK_GUTTER = "\u2502  "


def _print_block(prefix: str, text: str) -> None:
    """Print a possibly multi-line block, prefixing every line with *prefix*.

    Uses `Console.out` (no markup interpretation) so arbitrary content -- file
    text, tool output, model prose -- renders verbatim under the gutter.
    """
    for line in text.splitlines() or [""]:
        err_console.out(f"{prefix}{line}")


REGISTRY: dict[str, type["Tool"]] = {}


class Tool:
    """Rendering logic for one tool call/result pair.

    Subclasses declare their name as a class attribute and override only the
    methods whose default behaviour doesn't fit.  The registry is populated
    automatically by __init_subclass__, so adding a new tool is just writing
    a new class -- no dispatch table to maintain by hand.
    """

    name: str = ""

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
        """Short outcome string that follows `->` on the result line.

        Template method: renders errors and non-dict payloads generically, and
        dispatches successful dict results to the overridable `_success_summary`
        hook so subclasses never restate the error guard.
        """
        if not is_error and isinstance(result, dict):
            return self._success_summary(result)
        if is_error:
            msg = result.get("error") if isinstance(result, dict) else None  # type: ignore[union-attr]
            return f"error: {msg or _safe_json(result)}"
        return _truncate(_safe_json(result), 80)

    def _success_summary(self, result: dict) -> str:
        """Summary for a guaranteed non-error dict result; override per tool."""
        return _truncate(_safe_json(result), 80)

    def print_call_body(self, args: dict, prefix: str = "") -> None:
        """Emit long-form detail about the call itself (e.g. a fork's full
        instruction). Default is nothing; override when the arguments carry
        content worth showing in full. Every emitted line must start with
        *prefix* so forked-agent output stays under its gutter.
        """

    def print_result_body(self, result: object, is_error: bool, prefix: str = "") -> None:
        """Emit long-form body content (e.g. file text, stdout).

        Template method: skips errors and non-dict payloads, then dispatches
        successful dict results to the overridable `_success_body` hook. The
        *prefix* is threaded through so forked-agent output stays under its
        gutter.
        """
        if not is_error and isinstance(result, dict):
            self._success_body(result, prefix)

    def _success_body(self, result: dict, prefix: str = "") -> None:
        """Emit long-form body for a guaranteed non-error dict result.

        Default is nothing; override when there is human-consumable multi-line
        output worth showing. Every emitted line must start with *prefix*.
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

    def _success_summary(self, result: dict) -> str:
        content = result.get("content", "")
        if not content:
            n_lines = 0
        elif content.endswith("\n"):
            n_lines = content.count("\n")
        else:
            n_lines = content.count("\n") + 1
        return f"success, {n_lines} line{'' if n_lines == 1 else 's'} read"

    def _success_body(self, result: dict, prefix: str = "") -> None:
        content = result.get("content", "")
        if content:
            _print_block(prefix, content.rstrip("\n"))


class WriteTool(Tool):
    name = "write"

    def call_signature(self, args: dict) -> str:
        path = args.get("path", "?")
        size = len(args.get("content", "").encode("utf-8"))
        return f'write("{path}", {size} bytes)'

    def _success_summary(self, result: dict) -> str:
        n_bytes = result.get("bytes_written", "?")
        return f"success, {n_bytes} byte{'' if n_bytes == 1 else 's'} written"


class EditTool(Tool):
    name = "edit"

    def call_signature(self, args: dict) -> str:
        return f'edit("{args.get("path", "?")}")'

    def _success_summary(self, result: dict) -> str:
        return "success" if "bytes_written" in result else "unchanged"


class InsertTool(Tool):
    name = "insert"

    def call_signature(self, args: dict) -> str:
        return f'insert("{args.get("path", "?")}", after line {args.get("after_line", "?")})'

    def _success_summary(self, result: dict) -> str:
        n = result.get("lines_inserted", 0)
        return f"success, {n} line{'' if n == 1 else 's'} inserted"


class ExecBashTool(Tool):
    name = "exec_bash"

    def call_signature(self, args: dict) -> str:
        cmd = _truncate(args.get("command", ""), 80)
        return f"exec_bash({json.dumps(cmd)})"

    def _success_summary(self, result: dict) -> str:
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
                parts.append(f"unknown status: {json.dumps(status)}")
        else:
            parts.append(str(status))
        if result.get("stdout_truncated"):
            parts.append("stdout truncated")
        if result.get("stderr_truncated"):
            parts.append("stderr truncated")
        return ", ".join(parts)

    def _success_body(self, result: dict, prefix: str = "") -> None:
        stdout = result.get("stdout", "")
        stderr = result.get("stderr", "")
        if stdout:
            err_console.print(f"{prefix}[dim]--- stdout ---[/]")
            _print_block(prefix, stdout)
        if stderr:
            err_console.print(f"{prefix}[dim]--- stderr ---[/]")
            _print_block(prefix, stderr)


class AddTool(Tool):
    name = "add"

    def call_signature(self, args: dict) -> str:
        return f"add({args.get('a', '?')}, {args.get('b', '?')})"

    def _success_summary(self, result: dict) -> str:
        return f"sum={result.get('sum', '?')}"


class ForkTool(Tool):
    name = "fork"

    def call_signature(self, args: dict) -> str:
        parts: list[str] = []
        handle = args.get("handle")
        if handle:
            parts.append(f"handle={handle}")
        tools = args.get("tools")
        if tools is not None:
            parts.append(f"tools={json.dumps(tools)}")
        return f"fork({', '.join(parts)})"

    def print_call_body(self, args: dict, prefix: str = "") -> None:
        instruction = str(args.get("instruction", ""))
        err_console.print(f"{prefix}[dim]--- fork instruction (full prompt) ---[/]")
        _print_block(prefix, instruction if instruction else "(no instruction)")

    def _success_summary(self, result: dict) -> str:
        aid = str(result.get("agent_id", "?"))[:8]
        return f"spawned agent #{aid} (forked from {result.get('handle', '?')})"


class ExitTool(Tool):
    name = "exit"

    def call_signature(self, args: dict) -> str:
        return "exit()"

    def _success_summary(self, result: dict) -> str:
        return str(result.get("status", "exiting"))


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


def drain_events(
    proc: subprocess.Popen,
    pending_calls: dict[str, dict],
    main_agent_id: str,
) -> bool:
    """Forward a turn's events until the messaged agent and every peer idle.

    Each `agent.event` is tagged with its originating `agent_id`. A turn is not
    over when the messaged agent idles -- it may have `fork`ed a peer that is
    still working -- so we track an active set (the messaged agent plus every
    peer announced via `agent_forked`) and return only once all have gone idle.
    Forked-peer events (id != `main_agent_id`) render under a box-drawing gutter
    so they are visually distinct.

    `pending_calls` maps tool_use_id -> the call's arguments dict, so a
    tool_result event can replay the same call signature on its summary line.

    Returns True when the main (root) agent called `exit()` during the turn,
    signalling the REPL to shut down. An exiting agent stops its run loop
    without emitting a final `idle`, so its `exit` tool_result is what retires
    it from the active set; waiting for an idle that never arrives would hang.
    """
    assert proc.stdout is not None
    active: set[str] = {main_agent_id}
    forked: set[str] = set()
    root_exited = False
    # Streamed prose per forked peer, buffered and flushed under the gutter at
    # its idle; streaming a peer's tokens inline would interleave illegibly with
    # the messaged agent's own stream.
    prose: dict[str, str] = {}
    while active:
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
        method = msg.get("method")
        params = msg.get("params")
        if method == "skills_loaded":
            skills = params.get("skills", []) if isinstance(params, dict) else []
            names = ", ".join(str(s.get("name", "?")) for s in skills)
            suffix = f": {names}" if names else ""
            err_console.print(f"[dim]Loaded {len(skills)} skill(s){suffix}[/]")
            continue
        if method == "skills_diagnostics":
            count = len(params) if isinstance(params, list) else 0
            if count:
                err_console.print(f"[dim]Skills diagnostics: {count} item(s)[/]")
            continue
        if method == "agent_forked":
            if isinstance(params, dict) and params.get("agent_id"):
                fid = str(params["agent_id"])
                forked.add(fid)
                active.add(fid)
                origin = str(params.get("origin", ""))[:8]
                handle = params.get("handle", "?")
                # Leading newline closes any in-progress streamed line from the
                # messaged agent before the peer's framing begins.
                err_console.print(
                    f"\n{FORK_GUTTER}[bold magenta]┌─ forked agent #{fid[:8]}[/]"
                    f" [dim](from #{origin}, at {handle})[/]"
                )
            continue
        if method != "agent.event":
            print(f"[drain_events] ignoring notification with method={method!r}", file=sys.stderr)
            continue
        if not isinstance(params, dict):
            print("[drain_events] agent.event has non-dict params; ignoring", file=sys.stderr)
            continue
        event_type = params.get("type", "")
        aid = str(params.get("agent_id", ""))
        agent_short = aid[:8]
        prefix = "" if aid == main_agent_id else FORK_GUTTER
        if event_type == "token_delta":
            text = params.get("data", {}).get("text", "")
            if prefix:
                prose[aid] = prose.get(aid, "") + text
            else:
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
                lead = "" if prefix else "\n"
                err_console.print(
                    f"{lead}{prefix}[bold cyan]>>>[/] [bold]Agent #{agent_short}[/]: {tool.call_signature(args)}"
                )
                tool.print_call_body(args, prefix)
            elif block_type == "tool_result":
                name = data.get("name", "?")
                result = data.get("result")
                is_error = bool(data.get("is_error", False))
                tool_use_id = data.get("tool_use_id")
                args = pending_calls.pop(tool_use_id, {}) if tool_use_id else {}
                tool = Tool.from_name(name)
                marker_color = "red" if is_error else "green"
                err_console.print(
                    f"{prefix}[bold {marker_color}]>>>[/] [bold]Agent #{agent_short}[/]: "
                    f"{tool.call_signature(args)} -> {tool.result_summary(result, is_error)}"
                )
                tool.print_result_body(result, is_error, prefix)
                if name == "exit" and not is_error:
                    # exit stops the calling agent's run loop; it emits no
                    # following idle, so retire it here or drain would hang.
                    active.discard(aid)
                    if aid == main_agent_id:
                        root_exited = True
                        print()  # close the messaged agent's streamed line
                    elif aid in forked:
                        buffered = prose.pop(aid, "")
                        if buffered.strip():
                            _print_block(prefix, buffered.rstrip("\n"))
                        err_console.print(
                            f"{FORK_GUTTER}[bold magenta]└─ agent #{agent_short} exited[/]"
                        )
            # other block types are observation-only; nothing to render here
        elif event_type == "error":
            error_msg = params.get("data", {}).get("message", "unknown error")
            print(f"\n{prefix}Agent #{agent_short} error: {error_msg}", file=sys.stderr)
            active.discard(aid)
            if aid in forked:
                err_console.print(f"{FORK_GUTTER}[bold magenta]└─ agent #{agent_short} ended (error)[/]")
        elif event_type == "idle":
            if prefix:
                buffered = prose.pop(aid, "")
                if buffered.strip():
                    _print_block(prefix, buffered.rstrip("\n"))
                err_console.print(f"{FORK_GUTTER}[bold magenta]└─ agent #{agent_short} idle[/]")
            else:
                print()  # newline after the messaged agent's streamed output
            active.discard(aid)
    return root_exited


def main() -> None:
    project_dir = pathlib.Path(__file__).parent

    parser = argparse.ArgumentParser(description="Chat REPL for the Pristine agent harness.")
    parser.add_argument("invocation_dir", nargs="?", default=None)
    parser.add_argument("--model", default=None)
    parser.add_argument("-c", "--config", default=None)
    parser.add_argument("--trust-project-skills", action="store_true", default=None)
    args = parser.parse_args()
    invocation_dir = args.invocation_dir

    binary = project_dir / "target" / "debug" / "pristine"

    command = [str(binary), "run"]
    if args.model is not None:
        command += ["--model", args.model]
    if args.config is not None:
        command += ["-c", args.config]
    if args.trust_project_skills:
        command += ["--trust-project-skills"]

    proc = subprocess.Popen(
        command,
        cwd=invocation_dir,
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
            if drain_events(proc, pending_calls, agent_id):
                print("Agent exited; shutting down.", file=sys.stderr)
                break
    finally:
        if proc.poll() is None:
            try:
                send(proc, "shutdown")
            except (RpcError, OSError, SystemExit) as e:
                print(f"shutdown error: {e}", file=sys.stderr)
        proc.stdin.close()
        proc.wait()


if __name__ == "__main__":
    main()
