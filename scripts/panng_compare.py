#!/usr/bin/env python3
"""Compare real finite tools with a deterministic provider and identical work graphs.

Reports latency and request bytes, not model quality, actual token billing or prices.
Baselines are native response barriers, native background polling, and Panng.
"""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def run(binary, mode, verbose, evidence):
    requests, errors = [], []
    first_request = None
    with tempfile.TemporaryDirectory(prefix="panng-compare-") as temp:
        root = Path(temp)
        home, workspace = root / "home", root / "workspace"
        home.mkdir()
        workspace.mkdir()
        issued_followup = False
        polled = False

        class Provider(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                nonlocal issued_followup, polled, first_request
                try:
                    raw = self.rfile.read(int(self.headers["Content-Length"]))
                    if self.headers.get("Content-Encoding") == "zstd":
                        raw = subprocess.run(
                            ["zstd", "-dq", "-c"],
                            input=raw,
                            capture_output=True,
                            check=True,
                        ).stdout
                    body = json.loads(raw)
                    if first_request is None:
                        first_request = time.monotonic()
                    requests.append(body)
                    assert len(requests) <= 8, "unexpected scheduler loop"
                    calls = []

                    def command(call_id, shell, background=False):
                        arguments = {
                            "cmd": shell,
                            "login": False,
                            "max_output_tokens": 20000,
                        }
                        if mode == "panng":
                            arguments["timeout_ms"] = 10000
                        else:
                            arguments["yield_time_ms"] = 1 if background else 10000
                        return {
                            "type": "function_call",
                            "call_id": call_id,
                            "name": "exec_command",
                            "arguments": json.dumps(arguments),
                        }

                    if len(requests) == 1:
                        log = (
                            "awk 'BEGIN {for(i=0;i<2000;i++) print \"compilation diagnostic detail line\"}'"
                            if verbose
                            else "echo build-complete"
                        )
                        calls = [
                            command(
                                "inventory",
                                "sleep 0.1; echo inventory > inventory.txt; cat inventory.txt",
                            ),
                            command(
                                "build",
                                f"sleep 2; echo build > build.txt; {log}",
                                mode == "poll",
                            ),
                        ]
                    elif not issued_followup:
                        assert (workspace / "inventory.txt").exists(), (
                            "dependent work started before its input"
                        )
                        issued_followup = True
                        calls = [
                            command(
                                "lint",
                                "test -s inventory.txt && sleep 1.5 && echo lint > lint.txt; cat lint.txt",
                            )
                        ]
                    elif mode == "poll" and not polled:
                        sessions = re.findall(
                            r"Process running with session ID (\d+)",
                            json.dumps(body.get("input", [])),
                        )
                        if sessions:
                            polled = True
                            calls = [
                                {
                                    "type": "function_call",
                                    "call_id": "collect-build",
                                    "name": "write_stdin",
                                    "arguments": json.dumps(
                                        {
                                            "session_id": int(sessions[-1]),
                                            "chars": "",
                                            "yield_time_ms": 1000,
                                            "max_output_tokens": 20000,
                                        }
                                    ),
                                }
                            ]
                    ready = all(
                        (workspace / name).exists()
                        for name in ["inventory.txt", "build.txt", "lint.txt"]
                    )
                    if not calls:
                        calls = [
                            {
                                "type": "message",
                                "id": f"message-{len(requests)}",
                                "role": "assistant",
                                "content": [
                                    {
                                        "type": "output_text",
                                        "text": "DONE"
                                        if ready
                                        else "Waiting for the remaining build.",
                                    }
                                ],
                            }
                        ]
                    time.sleep(0.1)  # Fixed provider latency in every baseline.
                    response_id = f"response-{len(requests)}"
                    events = [
                        {"type": "response.created", "response": {"id": response_id}}
                    ]
                    events += [
                        {
                            "type": "response.output_item.done",
                            "output_index": index,
                            "item": call,
                        }
                        for index, call in enumerate(calls)
                    ]
                    events.append(
                        {
                            "type": "response.completed",
                            "response": {
                                "id": response_id,
                                "status": "completed",
                                "output": calls,
                            },
                        }
                    )
                    payload = "".join(
                        f"event: {event['type']}\ndata: {json.dumps(event)}\n\n"
                        for event in events
                    ).encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Content-Length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)
                except Exception as error:
                    errors.append(str(error))
                    self.send_error(500, "benchmark fixture failed")

        server = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        (home / "config.toml").write_text(f"""model = "gpt-5.5"
model_provider = "comparison"
approval_policy = "never"
sandbox_mode = "workspace-write"
[features]
code_mode = false
code_mode_only = false
enable_request_compression = false
apps = false
plugins = false
memories = false
[model_providers.comparison]
name = "comparison"
base_url = "http://127.0.0.1:{server.server_port}/v1"
wire_api = "responses"
requires_openai_auth = false
request_max_retries = 0
stream_max_retries = 0
""")
        evidence.mkdir(parents=True, exist_ok=True)
        try:
            began = time.monotonic()
            result = subprocess.run(
                [
                    str(binary),
                    "--no-daemon",
                    "-c",
                    f"experimental_panng={'true' if mode == 'panng' else 'false'}",
                    "exec",
                    "--skip-git-repo-check",
                    "--json",
                    "Run inventory and build independently, then lint using inventory. Finish after all three complete.",
                ],
                cwd=workspace,
                env={**os.environ, "CODEX_HOME": str(home)},
                input="",
                text=True,
                capture_output=True,
                timeout=60,
            )
            ended = time.monotonic()
            (evidence / "stdout.jsonl").write_text(result.stdout)
            (evidence / "stderr.log").write_text(result.stderr)
            (evidence / "requests.json").write_text(json.dumps(requests, indent=2))
            assert result.returncode == 0 and not errors, (
                result.returncode,
                errors,
                result.stderr[-1500:],
            )
            for name in ["inventory", "build", "lint"]:
                assert (workspace / f"{name}.txt").read_text() == f"{name}\n", name
            summary = {
                "mode": mode,
                "verbose_build_log": verbose,
                "requests": len(requests),
                "total_seconds": ended - began,
                "execution_seconds": ended - first_request,
                "input_message_bytes": sum(
                    len(
                        json.dumps(
                            request.get("input", []), ensure_ascii=False
                        ).encode()
                    )
                    for request in requests
                ),
                "tool_schema_bytes": sum(
                    len(
                        json.dumps(
                            request.get("tools", []), ensure_ascii=False
                        ).encode()
                    )
                    for request in requests
                ),
                "passed": True,
            }
            (evidence / "summary.json").write_text(json.dumps(summary, indent=2))
            return summary
        finally:
            server.shutdown()
            server.server_close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "binary", type=Path, help="Package bin/codex, not the Panng launcher"
    )
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--repeats", type=int, default=3)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    results = []
    for verbose in [False, True]:
        for iteration in range(args.repeats):
            modes = ["barrier", "poll", "panng"]
            modes = modes[iteration % 3 :] + modes[: iteration % 3]
            for mode in modes:
                result = run(
                    binary,
                    mode,
                    verbose,
                    args.evidence_dir
                    / f"{'verbose' if verbose else 'short'}-{iteration}-{mode}",
                )
                results.append(result)
                print(json.dumps(result), flush=True)
    (args.evidence_dir / "summary.json").write_text(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
