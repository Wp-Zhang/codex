#!/usr/bin/env python3
"""Exercise a packaged CLI against deterministic Responses SSE and real local tools.

This tests installation and tool execution, not model quality or provider billing.
All writes and configuration stay in a fresh temporary directory.
"""

import argparse
from collections import Counter
import json
from pathlib import Path
import os
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--evidence-dir", type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    evidence = args.evidence_dir.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    requests = []
    errors = []

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
                index = len(requests)
                requests.append(body)
                if index > 0 and "<panng_completion>" not in json.dumps(body):
                    raise AssertionError("next request lacks durable Panng completion")
                if index == 0:
                    item = {
                        "type": "function_call",
                        "call_id": "create",
                        "name": "exec_command",
                        "arguments": json.dumps(
                            {
                                "cmd": "echo before > smoke.txt",
                                "timeout_ms": 10000,
                                "login": False,
                            }
                        ),
                    }
                elif index == 1:
                    item = {
                        "type": "custom_tool_call",
                        "call_id": "patch",
                        "name": "apply_patch",
                        "input": "*** Begin Patch\n*** Update File: smoke.txt\n@@\n-before\n+after\n*** End Patch",
                    }
                elif index == 2:
                    item = {
                        "type": "function_call",
                        "call_id": "verify",
                        "name": "exec_command",
                        "arguments": json.dumps(
                            {
                                "cmd": "cat smoke.txt",
                                "timeout_ms": 10000,
                                "login": False,
                            }
                        ),
                    }
                elif index == 3:
                    item = {
                        "type": "message",
                        "role": "assistant",
                        "id": "done",
                        "content": [
                            {"type": "output_text", "text": "Package smoke completed."}
                        ],
                    }
                else:
                    raise AssertionError("unexpected extra model request")
                response_id = f"smoke-{index}"
                events = [
                    {"type": "response.created", "response": {"id": response_id}},
                    {"type": "response.output_item.done", "item": item},
                    {"type": "response.completed", "response": {"id": response_id}},
                ]
                data = "".join(
                    f"data: {json.dumps(event)}\n\n" for event in events
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
    try:
        with tempfile.TemporaryDirectory(prefix="panng-package-smoke-") as temp:
            root = Path(temp)
            home, workspace = root / "home", root / "workspace"
            home.mkdir()
            workspace.mkdir()
            config = [
                "experimental_panng=true",
                "features.code_mode=false",
                "features.code_mode_only=false",
                'model="gpt-6-sol"',
                'model_provider="panng_smoke"',
                'approval_policy="never"',
                'model_providers.panng_smoke={name="Package smoke",wire_api="responses",'
                f'base_url="http://127.0.0.1:{server.server_port}/v1",requires_openai_auth=false}}',
            ]
            command = [
                str(binary),
                "exec",
                "--skip-git-repo-check",
                "--json",
                "--sandbox",
                "workspace-write",
            ]
            for setting in config:
                command.extend(["-c", setting])
            command.append(
                "Create smoke.txt containing before, patch it to after, then read it back."
            )
            result = subprocess.run(
                command,
                cwd=workspace,
                env={**os.environ, "CODEX_HOME": str(home)},
                capture_output=True,
                text=True,
                timeout=180,
            )
            (evidence / "stdout.jsonl").write_text(result.stdout)
            (evidence / "stderr.log").write_text(result.stderr)
            (evidence / "requests.json").write_text(json.dumps(requests, indent=2))
            assert result.returncode == 0, result.stderr
            assert not errors, errors
            assert len(requests) == 4, len(requests)
            assert (workspace / "smoke.txt").read_text() == "after\n"
            journals = list((home / "sessions").rglob("*.jsonl"))
            assert journals, "no durable journal"
            assert any('"panng_v1"' in path.read_text() for path in journals)
            effects = []
            for path in journals:
                journal = path.read_text()
                (evidence / path.name).write_text(journal)
                for line in journal.splitlines():
                    record = json.loads(line)
                    if record.get("type") == "panng_commit":
                        effects.extend(record["payload"]["host_effects"])
            events = [
                effect["payload"] for effect in effects if effect["type"] == "event_msg"
            ]
            completed = Counter(
                event["item"]["type"]
                for event in events
                if event["type"] == "item_completed"
            )
            summary = {
                "completed_items": dict(completed),
                "turn_completions": sum(
                    event["type"] == "task_complete" for event in events
                ),
                "native_append_refusals": result.stderr.count(
                    "refusing native rollout append"
                ),
            }
            (evidence / "canonical-summary.json").write_text(
                json.dumps(summary, indent=2)
            )
            assert (
                completed["CommandExecution"] == 2 and completed["FileChange"] == 1
            ), summary
            assert completed["UserMessage"] == 1 and completed["AgentMessage"] == 1, (
                summary
            )
            assert summary["turn_completions"] == 1, summary
            assert summary["native_append_refusals"] == 0, summary
            print(
                json.dumps(
                    {
                        "binary": str(binary),
                        "requests": len(requests),
                        "real_file_result": "after",
                        "canonical": summary,
                        "passed": True,
                    }
                )
            )
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
