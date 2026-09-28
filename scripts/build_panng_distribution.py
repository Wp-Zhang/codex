#!/usr/bin/env python3
"""Package prebuilt fork binaries using the upstream Codex package assembler."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--codex-bin", type=Path, required=True)
    parser.add_argument("--code-mode-host-bin", type=Path, required=True)
    parser.add_argument("--zsh-bin", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    if not all(part.isalnum() or part in ".-" for part in args.version):
        parser.error("version must contain only letters, digits, dots and hyphens")
    repo = Path(__file__).resolve().parent.parent
    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=True)
    name = f"codex-panng-{args.version}-macos-arm64"
    package = output / name
    if package.exists():
        parser.error(f"output already exists: {package}")
    subprocess.run(
        [
            sys.executable,
            str(repo / "scripts/build_codex_package.py"),
            "--target",
            "aarch64-apple-darwin",
            "--entrypoint-bin",
            str(args.codex_bin.resolve(strict=True)),
            "--code-mode-host-bin",
            str(args.code_mode_host_bin.resolve(strict=True)),
            "--zsh-bin",
            str(args.zsh_bin.resolve(strict=True)),
            "--package-version",
            args.version,
            "--package-dir",
            str(package),
        ],
        cwd=repo,
        env={**os.environ, "CODEX_REPO_ROOT": str(repo)},
        check=True,
    )
    for binary in [package / "bin/codex", package / "bin/codex-code-mode-host"]:
        subprocess.run(["/usr/bin/strip", "-S", "-x", str(binary)], check=True)
        subprocess.run(
            [
                "/usr/bin/codesign",
                "--force",
                "--sign",
                "-",
                "--timestamp=none",
                str(binary),
            ],
            check=True,
        )
    launcher = package / "bin/codex-panng"
    launcher.write_text(f"""#!/bin/sh
set -eu
if [ "${{1:-}}" = "--version" ]; then
    echo "codex-panng {args.version}"
    exit 0
fi
panng_bin_dir=$(cd "$(dirname "$0")" >/dev/null && pwd)
exec "$panng_bin_dir/codex" --no-daemon -c experimental_panng=true -c check_for_update_on_startup=false \\
  -c features.code_mode=false -c features.code_mode_only=false "$@"
""")
    launcher.chmod(0o755)
    shutil.copy2(repo / "LICENSE", package / "LICENSE")
    commit = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=repo, text=True
    ).strip()
    dirty = bool(
        subprocess.check_output(
            ["git", "status", "--porcelain"], cwd=repo, text=True
        ).strip()
    )
    (package / "panng-build.json").write_text(
        json.dumps(
            {
                "version": args.version,
                "target": "aarch64-apple-darwin",
                "commit": commit,
                "working_tree_dirty": dirty,
                "profile": "dev, stripped",
                "launcher": "bin/codex-panng",
            },
            indent=2,
        )
    )
    (package / "README.md").write_text(f"""# Codex Panng {args.version}

macOS Apple Silicon。保留整个解压目录，将它放到任意稳定位置。

启动交互界面：

```sh
./bin/codex-panng -m gpt-5.5
```

使用已有 Codex 登录和配置。`gpt-5.5` 是本包真实账户验证使用的模型；可以用 `-m` 选择账户支持的其他模型。
恢复对话：`./bin/codex-panng -m gpt-5.5 resume`。运行中按 Escape 中断，再输入新请求继续。
退出使用 `/quit`。`./bin/codex-panng --version` 显示 fork 版本。

如需独立配置与历史，先设置 `CODEX_HOME` 并运行 `./bin/codex login` 登录。
启动器开启 Panng，关闭另一套 Code Mode 执行机制。直接运行 `./bin/codex` 使用原生入口。
源码版本及构建状态见 `panng-build.json`；文件校验见 `SHA256SUMS`。
""")
    files = sorted(path for path in package.rglob("*") if path.is_file())
    checksums = []
    for path in files:
        with path.open("rb") as source:
            digest = hashlib.file_digest(source, "sha256").hexdigest()
        checksums.append(f"{digest}  {path.relative_to(package)}\n")
    (package / "SHA256SUMS").write_text("".join(checksums))
    archive = output / f"{name}.tar.gz"
    with tarfile.open(archive, "w:gz") as tar:
        tar.add(package, arcname=name)
    with archive.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    archive.with_suffix(archive.suffix + ".sha256").write_text(
        f"{digest}  {archive.name}\n"
    )
    print(
        json.dumps({"package": str(package), "archive": str(archive), "sha256": digest})
    )


if __name__ == "__main__":
    main()
