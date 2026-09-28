#!/usr/bin/env python3
"""Drive a packaged Panng TUI through real PTY input, tools, interrupt and resume.

Unix/macOS only. Uses the standard library and a local deterministic Responses
server; it tests the product path, not model quality or provider billing.
"""

import argparse
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import tempfile
import termios
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


PROMPTS = {
    "ONE": "TUI_TURN_ONE: Write alpha-remembered to memo.txt and read it back.",
    "TWO": "TUI_TURN_TWO: Use the first turn's result and copy memo.txt to remembered.txt.",
    "CANCEL": "TUI_TURN_CANCEL: Start the long cancellable command now.",
    "THREE": "TUI_TURN_THREE: After cancellation, copy the remembered first result to after.txt.",
    "RESUME": "TUI_TURN_RESUME: Use our previous turns and copy after.txt to resumed.txt.",
    "FORK": "TUI_TURN_FORK: In this fork, use the inherited result and copy resumed.txt to forked.txt.",
}
COMMANDS = {
    "ONE": "printf 'alpha-remembered\\n' > memo.txt; cat memo.txt",
    "TWO": "cat memo.txt > remembered.txt; cat remembered.txt",
    "CANCEL": "printf started > cancel.started; while [ ! -f cancel.release ]; do sleep 0.1; done; printf forbidden > late-success.txt",
    "THREE": "cat remembered.txt > after.txt; cat after.txt",
    "RESUME": "cat after.txt > resumed.txt; cat resumed.txt",
    "FORK": "cat resumed.txt > forked.txt; cat forked.txt",
}
CALLS = {stage: f"tui-{stage.lower()}" for stage in PROMPTS}
TERMINALS = {"task_complete", "turn_complete", "turn_aborted"}


def text_only(data):
    text = data.decode("utf-8", errors="replace")
    text = re.sub(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)", "", text)
    return re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", text)


class Terminal:
    def __init__(self, command, cwd, env, evidence, name):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 140, 0, 0))
        self.process = subprocess.Popen(
            command,
            cwd=cwd,
            env=env,
            stdin=slave,
            stdout=slave,
            stderr=slave,
            start_new_session=True,
        )
        os.close(slave)
        self.raw = bytearray()
        self.probe_tail = b""
        self.evidence, self.name = evidence, name
        (evidence / f"{name}.pty").write_bytes(b"")
        self.closed = False

    def pump(self, timeout=0.1):
        if self.closed or not select.select([self.master], [], [], timeout)[0]:
            return
        try:
            chunk = os.read(self.master, 65536)
        except OSError as error:
            if error.errno != errno.EIO:
                raise
            chunk = b""
        if not chunk:
            self.closed = True
            return
        self.raw.extend(chunk)
        with (self.evidence / f"{self.name}.pty").open("ab") as log:
            log.write(chunk)
        # Answer only actual terminal probes, including probes split across reads.
        data = self.probe_tail + chunk
        probes = [
            (rb"\x1b\[6n", b"\x1b[1;1R"),
            (rb"\x1b\[\?u", b"\x1b[?0u"),
            (rb"\x1b\[>c", b"\x1b[>0;0;0c"),
            (rb"\x1b\[c", b"\x1b[?1;2c"),
            (rb"\x1b\]10;\?(?:\x07|\x1b\\)", b"\x1b]10;rgb:dddd/dddd/dddd\x1b\\"),
            (rb"\x1b\]11;\?(?:\x07|\x1b\\)", b"\x1b]11;rgb:1111/1111/1111\x1b\\"),
        ]
        for pattern, response in probes:
            for match in re.finditer(pattern, data):
                if match.end() > len(self.probe_tail):
                    os.write(self.master, response)
        self.probe_tail = data[-40:]

    def wait(self, predicate, description, timeout=45):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.pump()
            status = self.process.poll()
            if predicate():
                return
            visible = text_only(self.raw)
            if "Failed to start turn:" in visible:
                raise AssertionError(
                    f"TUI rejected input while {description}: {visible[-2500:]}"
                )
            if status is not None:
                raise AssertionError(
                    f"TUI exited ({status}) while {description}: {text_only(self.raw)[-2500:]}"
                )
        raise AssertionError(
            f"Timeout while {description}: {text_only(self.raw)[-2500:]}"
        )

    def settle(self, seconds=0.4):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            self.pump()

    def submit(self, text):
        os.write(self.master, b"\x1b[200~" + text.encode() + b"\x1b[201~")
        self.settle(0.2)
        os.write(self.master, b"\r")

    def exit(self):
        self.submit("/quit")
        self.wait(lambda: self.process.poll() is not None, "exiting TUI", timeout=20)
        if self.process.returncode != 0:
            raise AssertionError(f"TUI exit code {self.process.returncode}")

    def close(self):
        if self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGTERM)
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait()
        self.settle(0.1)
        (self.evidence / f"{self.name}.pty").write_bytes(self.raw)
        (self.evidence / f"{self.name}.txt").write_text(text_only(self.raw))
        os.close(self.master)


def journal(home, thread_id=None):
    for path in sorted((home / "sessions").rglob("*.jsonl")):
        raw = path.read_bytes()
        lines = [json.loads(line) for line in raw.split(b"\n")[:-1] if line]
        if lines and lines[0].get("payload", {}).get("history_mode") == "panng_v1":
            if thread_id is None or lines[0]["payload"]["id"] == thread_id:
                return path, lines
    return None, []


def events(home, thread_id=None):
    _, lines = journal(home, thread_id)
    return [
        (line.get("ordinal"), effect["payload"])
        for line in lines
        if line.get("type") == "panng_commit"
        for effect in line["payload"]["host_effects"]
        if effect.get("type") == "event_msg"
    ]


def terminal_events(home, thread_id=None):
    return [
        (ordinal, event)
        for ordinal, event in events(home, thread_id)
        if event["type"] in TERMINALS
    ]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path, help="Package bin/codex-panng launcher")
    parser.add_argument("--evidence-dir", type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    evidence = args.evidence_dir.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    requests, errors, stage_counts = [], [], {}
    lock = threading.Lock()

    class Provider(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            try:
                raw = self.rfile.read(int(self.headers["Content-Length"]))
                if self.headers.get("Content-Encoding") == "zstd":
                    raw = subprocess.run(
                        ["zstd", "-d", "-q", "-c"],
                        input=raw,
                        capture_output=True,
                        check=True,
                    ).stdout
                body = json.loads(raw)
                # Match the latest actual user prompt, not a marker in tool output.
                stage = None
                for item in body.get("input", []):
                    if item.get("role") == "user":
                        serialized = json.dumps(item)
                        for candidate in PROMPTS:
                            if f"TUI_TURN_{candidate}:" in serialized:
                                stage = candidate
                assert stage is not None, "request lacks a submitted TUI prompt"
                with lock:
                    index = stage_counts.get(stage, 0)
                    stage_counts[stage] = index + 1
                    requests.append({"stage": stage, "step": index, "body": body})
                    (evidence / "requests.json").write_text(
                        json.dumps(requests, indent=2)
                    )
                serialized = json.dumps(body)
                if stage in {"TWO", "THREE", "RESUME", "FORK"}:
                    assert "TUI_ONE_DONE" in serialized, (
                        "first assistant result lost between turns"
                    )
                    assert "alpha-remembered" in serialized, (
                        "first tool result lost between turns"
                    )
                if stage in {"RESUME", "FORK"}:
                    assert "TUI_THREE_DONE" in serialized, (
                        "resume lost completed prior turn"
                    )
                assert index < 2 and not (stage == "CANCEL" and index > 0), (
                    f"unexpected continuation {stage}/{index}"
                )
                if index == 0:
                    item = {
                        "type": "function_call",
                        "call_id": CALLS[stage],
                        "name": "exec_command",
                        "arguments": json.dumps(
                            {
                                "cmd": COMMANDS[stage],
                                "timeout_ms": 10000,
                                "login": False,
                            }
                        ),
                    }
                else:
                    assert "<panng_completion>" in serialized, (
                        "missing durable tool completion"
                    )
                    item = {
                        "type": "message",
                        "role": "assistant",
                        "id": f"done-{stage.lower()}",
                        "content": [
                            {
                                "type": "output_text",
                                "text": f"TUI_{stage}_DONE alpha-remembered",
                            }
                        ],
                    }
                response_id = f"tui-{stage.lower()}-{index}"
                sse = [
                    {"type": "response.created", "response": {"id": response_id}},
                    {"type": "response.output_item.done", "item": item},
                    {"type": "response.completed", "response": {"id": response_id}},
                ]
                data = "".join(
                    f"data: {json.dumps(event)}\n\n" for event in sse
                ).encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)
            except Exception as error:
                errors.append(str(error))
                self.send_error(500, str(error))

    server = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    terminals = []
    summary = {"binary": str(binary), "passed": False}
    try:
        with tempfile.TemporaryDirectory(prefix="panng-tui-smoke-") as temporary:
            root = Path(temporary).resolve()
            home, workspace = root / "home", root / "workspace"
            home.mkdir()
            workspace.mkdir()
            (home / "config.toml").write_text(f"""experimental_panng = true
model = "gpt-6-sol"
model_provider = "tui_smoke"
approval_policy = "never"
sandbox_mode = "workspace-write"
check_for_update_on_startup = false
suppress_unstable_features_warning = true
[projects.{json.dumps(str(workspace))}]
trust_level = "trusted"
[features]
code_mode = false
code_mode_only = false
enable_request_compression = false
memories = false
apps = false
plugins = false
[analytics]
enabled = false
[model_providers.tui_smoke]
name = "TUI smoke"
wire_api = "responses"
base_url = "http://127.0.0.1:{server.server_port}/v1"
requires_openai_auth = false
request_max_retries = 0
stream_max_retries = 0
""")
            env = {
                **os.environ,
                "CODEX_HOME": str(home),
                "TERM": "xterm-256color",
                "ZDOTDIR": str(root),
            }
            env.pop("CODEX_SQLITE_HOME", None)
            env.pop("BASH_ENV", None)
            env["NO_PROXY"] = ",".join(
                filter(None, [env.get("NO_PROXY", ""), "127.0.0.1", "localhost"])
            )
            try:
                tui = Terminal(
                    [str(binary), "--no-alt-screen"],
                    workspace,
                    env,
                    evidence,
                    "initial",
                )
                terminals.append(tui)
                tui.wait(
                    lambda: (
                        bool(journal(home)[1])
                        and "gpt-6-sol" in text_only(tui.raw).lower()
                    ),
                    "waiting for TUI startup",
                )
                tui.settle()
                for stage, expected in [("ONE", 1), ("TWO", 2)]:
                    tui.submit(PROMPTS[stage])
                    tui.wait(
                        lambda: len(terminal_events(home)) == expected,
                        f"completing {stage}",
                    )
                    tui.settle()
                tui.submit(PROMPTS["CANCEL"])
                tui.wait(
                    lambda: (workspace / "cancel.started").exists(),
                    "starting cancellable real command",
                )
                os.write(tui.master, b"\x1b")  # Escape is the normal TUI interrupt key.
                tui.wait(
                    lambda: len(terminal_events(home)) == 3,
                    "recording interrupted turn",
                )
                assert terminal_events(home)[-1][1]["type"] == "turn_aborted", (
                    terminal_events(home)
                )
                tui.settle()
                tui.submit(PROMPTS["THREE"])
                tui.wait(
                    lambda: len(terminal_events(home)) == 4,
                    "continuing after interruption",
                )
                tui.settle()
                thread_id = journal(home)[1][0]["payload"]["id"]
                tui.exit()
                initial_output = bytes(tui.raw)
                tui.close()
                terminals.remove(tui)
                resumed = Terminal(
                    [str(binary), "--no-alt-screen", "resume", thread_id],
                    workspace,
                    env,
                    evidence,
                    "resumed",
                )
                terminals.append(resumed)
                resumed.wait(
                    lambda: "gpt-6-sol" in text_only(resumed.raw).lower(),
                    "resuming the TUI",
                )
                resumed.settle(1)
                resumed.submit(PROMPTS["RESUME"])
                resumed.wait(
                    lambda: len(terminal_events(home)) == 5, "continuing resumed TUI"
                )
                resumed.settle()
                resumed.submit("/fork")

                def fork_id():
                    for candidate in (home / "sessions").rglob("*.jsonl"):
                        first = candidate.read_bytes().split(b"\n", 1)[0]
                        if not first:
                            continue
                        header = json.loads(first).get("payload", {})
                        if header.get("forked_from_id") == thread_id:
                            return header["id"]
                    return None

                resumed.wait(
                    lambda: fork_id() is not None, "forking through the real TUI"
                )
                child_id = fork_id()
                resumed.settle(1)
                resumed.submit(PROMPTS["FORK"])
                resumed.wait(
                    lambda: len(terminal_events(home, child_id)) == 1,
                    "continuing the TUI fork",
                )
                resumed.settle()
                resumed.exit()
                child_path, child_lines = journal(home, child_id)
                assert (
                    child_lines[1]["payload"]["mutation"]["Snapshot"]["reason"]
                    == "Fork"
                ), child_lines[1]
                assert (
                    child_lines[1]["payload"]["checkpoint"]["parent_cut"]["thread_id"]
                    == thread_id
                )
                child_tools = [
                    event["item"]
                    for _, event in events(home, child_id)
                    if event["type"] == "item_completed"
                    and event["item"].get("id") == CALLS["FORK"]
                ]
                assert len(child_tools) == 1 and child_tools[0].get("exit_code") == 0, (
                    child_tools
                )
                shutil.copy2(child_path, evidence / "fork-journal.jsonl")
                assert not errors, errors
                assert stage_counts == {
                    "ONE": 2,
                    "TWO": 2,
                    "CANCEL": 1,
                    "THREE": 2,
                    "RESUME": 2,
                    "FORK": 2,
                }, stage_counts
                for name in [
                    "memo.txt",
                    "remembered.txt",
                    "after.txt",
                    "resumed.txt",
                    "forked.txt",
                ]:
                    assert (workspace / name).read_text() == "alpha-remembered\n", name
                assert not (workspace / "late-success.txt").exists(), (
                    "cancelled process ran its success side effect"
                )
                all_events = events(home)
                ends = terminal_events(home)
                assert len({event["turn_id"] for _, event in ends}) == 5, ends
                assert sum(event["type"] == "turn_aborted" for _, event in ends) == 1, (
                    ends
                )
                assert all(event.get("error") is None for _, event in ends), ends
                abort_ordinal = next(
                    ordinal
                    for ordinal, event in ends
                    if event["type"] == "turn_aborted"
                )
                completed = [
                    (ordinal, event["item"])
                    for ordinal, event in all_events
                    if event["type"] == "item_completed"
                ]
                for stage in ["ONE", "TWO", "THREE", "RESUME"]:
                    matches = [
                        item for _, item in completed if item.get("id") == CALLS[stage]
                    ]
                    assert len(matches) == 1 and matches[0].get("exit_code") == 0, (
                        stage,
                        matches,
                    )
                cancelled_items = [
                    (ordinal, item)
                    for ordinal, item in completed
                    if item.get("id") == CALLS["CANCEL"]
                ]
                assert len(cancelled_items) == 1, (
                    "cancelled tool UI must be recoverable exactly once",
                    cancelled_items,
                )
                cancelled_ordinal, cancelled_item = cancelled_items[0]
                assert cancelled_ordinal < abort_ordinal, (
                    "cancelled-tool UI was persisted after turn abort"
                )
                assert (
                    cancelled_item.get("status") != "completed"
                    and cancelled_item.get("exit_code") != 0
                ), cancelled_item
                assert not [
                    item
                    for ordinal, item in completed
                    if ordinal > abort_ordinal and item.get("id") == CALLS["CANCEL"]
                ], "late cancelled-tool UI completion"
                transcript = text_only(initial_output) + text_only(resumed.raw)
                logs = "".join(
                    path.read_text(errors="replace")
                    for path in (home / "log").rglob("*.log")
                )
                assert "refusing native rollout append" not in transcript + logs, (
                    "native append refusal"
                )
                summary.update(
                    passed=True,
                    thread_id=thread_id,
                    fork_thread_id=child_id,
                    turns=len(ends) + 1,
                    requests=len(requests),
                    real_file_result="alpha-remembered",
                )
            finally:
                for terminal in terminals:
                    terminal.close()
                path, lines = journal(home)
                if path:
                    shutil.copy2(path, evidence / "journal.jsonl")
                    (evidence / "journal-events.json").write_text(
                        json.dumps(events(home), indent=2)
                    )
                shutil.copytree(workspace, evidence / "workspace", dirs_exist_ok=True)
                if (home / "log").exists():
                    shutil.copytree(home / "log", evidence / "logs", dirs_exist_ok=True)
    except Exception as error:
        summary["error"] = str(error)
        raise
    finally:
        server.shutdown()
        server.server_close()
        (evidence / "requests.json").write_text(json.dumps(requests, indent=2))
        (evidence / "provider-errors.json").write_text(json.dumps(errors, indent=2))
        (evidence / "summary.json").write_text(json.dumps(summary, indent=2))
        print(json.dumps(summary))


if __name__ == "__main__":
    main()
