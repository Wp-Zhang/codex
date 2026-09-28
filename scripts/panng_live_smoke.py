#!/usr/bin/env python3
"""Two real authenticated model turns with isolated files and cold CLI resume.

Uses existing Codex login credentials without copying them into evidence.
Unlike the deterministic package/TUI fixtures, this contacts the actual provider.
"""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--model", default="gpt-5.6-sol")
    parser.add_argument(
        "--tui",
        action="store_true",
        help="Drive the actual terminal UI instead of exec",
    )
    parser.add_argument("--evidence-dir", type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    evidence = args.evidence_dir.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    summary = {
        "model": args.model,
        "provider": "real authenticated provider",
        "tui": args.tui,
        "passed": False,
    }
    with tempfile.TemporaryDirectory(prefix="panng-live-") as temp:
        root = Path(temp).resolve()
        home, workspace = root / "home", root / "workspace"
        home.mkdir()
        workspace.mkdir()
        source_home = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex")))
        shutil.copyfile(source_home / "auth.json", home / "auth.json")
        (home / "auth.json").chmod(0o600)
        (home / "config.toml").write_text(
            f'model = {json.dumps(args.model)}\nmodel_reasoning_effort = "low"\n'
            'approval_policy = "never"\nsandbox_mode = "workspace-write"\n'
            "check_for_update_on_startup = false\nsuppress_unstable_features_warning = true\n"
            f'[projects.{json.dumps(str(workspace))}]\ntrust_level = "trusted"\n'
            "[features]\napps = false\nplugins = false\nmemories = false\n"
        )
        env = {**os.environ, "CODEX_HOME": str(home)}
        env.pop("CODEX_SQLITE_HOME", None)
        prompts = [
            "Create check.txt containing exactly live model validated followed by a newline. "
            "Then read the file using a command and verify it. Remember the code word amber-river. "
            "Only work in this directory and reply DONE when verified.",
            "Continue our earlier conversation. Create remembered.txt containing only the code word "
            "I told you previously, followed by a newline. Read both files with a command to verify "
            "them, then reply DONE. Do not change check.txt.",
        ]
        thread_id = None
        try:
            for index, prompt in enumerate(prompts):
                if args.tui:
                    sys.path.insert(0, str(Path(__file__).resolve().parent))
                    from panng_tui_smoke import (
                        Terminal,
                        journal,
                        terminal_events,
                        text_only,
                    )

                    command = [str(binary), "--no-alt-screen"]
                    if thread_id:
                        command += ["resume", thread_id]
                    terminal = Terminal(
                        command,
                        workspace,
                        {**env, "TERM": "xterm-256color"},
                        evidence,
                        f"turn-{index + 1}",
                    )
                    try:
                        handled = set()
                        for _ in range(3):
                            terminal.wait(
                                lambda: (
                                    args.model.lower()
                                    in text_only(terminal.raw).lower()
                                    and bool(journal(home)[1])
                                    or any(
                                        label not in handled
                                        and label
                                        in "".join(
                                            text_only(terminal.raw).lower().split()
                                        )
                                        for label in (
                                            "trustandcontinue",
                                            "useexistingmodel",
                                        )
                                    )
                                ),
                                "starting authenticated TUI",
                                timeout=45,
                            )
                            screen = text_only(terminal.raw)
                            compact = "".join(screen.lower().split())
                            if (
                                "useexistingmodel" in compact
                                and "useexistingmodel" not in handled
                            ):
                                handled.add("useexistingmodel")
                                os.write(terminal.master, b"\x1b[B\r")
                            elif (
                                "trustandcontinue" in compact
                                and "trustandcontinue" not in handled
                            ):
                                handled.add("trustandcontinue")
                                os.write(terminal.master, b"\r")
                            else:
                                break
                        terminal.settle()
                        terminal.submit(prompt)
                        terminal.wait(
                            lambda: len(terminal_events(home)) == index + 1,
                            "completing real model TUI turn",
                            timeout=180,
                        )
                        assert terminal_events(home)[-1][1].get("error") is None
                        observed_id = journal(home)[1][0]["payload"]["id"]
                        assert thread_id is None or thread_id == observed_id
                        thread_id = observed_id
                        terminal.settle()
                        terminal.exit()
                    finally:
                        terminal.close()
                    continue
                command = [str(binary), "exec"]
                if thread_id:
                    command += ["resume", thread_id]
                command += ["--skip-git-repo-check", "--json", prompt]
                try:
                    result = subprocess.run(
                        command,
                        cwd=workspace,
                        env=env,
                        input="",
                        capture_output=True,
                        text=True,
                        timeout=180,
                    )
                except subprocess.TimeoutExpired as error:
                    for name, content in [
                        ("jsonl", error.stdout),
                        ("stderr.log", error.stderr),
                    ]:
                        if isinstance(content, bytes):
                            content = content.decode(errors="replace")
                        (evidence / f"turn-{index + 1}.{name}").write_text(
                            content or ""
                        )
                    raise
                (evidence / f"turn-{index + 1}.jsonl").write_text(result.stdout)
                (evidence / f"turn-{index + 1}.stderr.log").write_text(result.stderr)
                assert result.returncode == 0, (
                    f"turn {index + 1} failed; see saved logs"
                )
                for line in result.stdout.splitlines():
                    event = json.loads(line)
                    if event.get("type") == "thread.started":
                        if thread_id:
                            assert event["thread_id"] == thread_id
                        thread_id = event["thread_id"]
                assert thread_id, "missing thread identity"
            assert (workspace / "check.txt").read_text() == "live model validated\n"
            assert (workspace / "remembered.txt").read_text() == "amber-river\n"
            summary.update(passed=True, thread_id=thread_id, turns=2)
        finally:
            for journal in (home / "sessions").rglob("*.jsonl"):
                shutil.copy2(journal, evidence / journal.name)
            shutil.copytree(workspace, evidence / "workspace", dirs_exist_ok=True)
            if (home / "log").exists():
                shutil.copytree(home / "log", evidence / "logs", dirs_exist_ok=True)
            (evidence / "summary.json").write_text(json.dumps(summary, indent=2))
            print(json.dumps(summary))


if __name__ == "__main__":
    main()
