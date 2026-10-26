#!/usr/bin/env python3
"""下载 open-code-review（ocr CLI）到 bin/ 目录（随包 sidecar，不入库）。

用法：
    python scripts/fetch_ocr.py [--version v1.13.3]

按当前平台选择 release 资产，下载后校验 sha256，失败即删除并报错。
离线时可手动从 https://github.com/alibaba/open-code-review/releases 下载
对应平台的二进制，放到 bin/ocr.exe（Windows）或 bin/ocr（其它系统）。
"""

import argparse
import hashlib
import os
import platform
import shutil
import stat
import sys
import tempfile
import urllib.request

OCR_VERSION = "v1.13.3"
REPO = "alibaba/open-code-review"

# (sys.platform, platform.machine().lower() 规范化) -> release 资产名
ASSETS = {
    ("windows", "amd64"): "opencodereview-windows-amd64.exe",
    ("windows", "arm64"): "opencodereview-windows-arm64.exe",
    ("linux", "amd64"): "opencodereview-linux-amd64",
    ("linux", "arm64"): "opencodereview-linux-arm64",
    ("darwin", "amd64"): "opencodereview-darwin-amd64",
    ("darwin", "arm64"): "opencodereview-darwin-arm64",
}

MACHINE_ALIASES = {
    "x86_64": "amd64",
    "amd64": "amd64",
    "aarch64": "arm64",
    "arm64": "arm64",
}


def detect_asset() -> str:
    plat = sys.platform
    if plat.startswith("win"):
        plat = "windows"
    elif plat.startswith("linux"):
        plat = "linux"
    machine = MACHINE_ALIASES.get(platform.machine().lower())
    asset = ASSETS.get((plat, machine))
    if asset is None:
        sys.exit(f"不支持的平台：{sys.platform}/{platform.machine()}，请手动下载 ocr 放入 bin/ 目录")
    return asset


def download(url: str, dest: str) -> None:
    req = urllib.request.Request(url, headers={"User-Agent": "pr-desk-fetch-ocr"})
    with urllib.request.urlopen(req, timeout=60) as resp, open(dest, "wb") as f:
        shutil.copyfileobj(resp, f)


def sha256_of(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def expected_hash(sums_text: str, asset: str) -> str:
    for line in sums_text.splitlines():
        parts = line.split()
        # 兼容 "<hash>  <file>" 与 "<hash>  *<file>"（sha256sum 二进制模式）
        if len(parts) == 2 and parts[1].lstrip("*") == asset:
            return parts[0].lower()
    sys.exit(f"sha256sum.txt 中找不到 {asset} 的哈希，已中止")


def main() -> None:
    parser = argparse.ArgumentParser(description="下载 ocr CLI 到 bin/ 目录")
    parser.add_argument("--version", default=OCR_VERSION, help=f"ocr release 版本（默认 {OCR_VERSION}）")
    args = parser.parse_args()

    asset = detect_asset()
    out_name = "ocr.exe" if asset.endswith(".exe") else "ocr"
    bin_dir = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "bin")
    os.makedirs(bin_dir, exist_ok=True)
    out_path = os.path.join(bin_dir, out_name)

    base = f"https://github.com/{REPO}/releases/download/{args.version}"
    print(f"下载 ocr {args.version}（{asset}）…")

    tmp_dir = tempfile.mkdtemp(prefix="prdesk-ocr-dl-")
    tmp_bin = os.path.join(tmp_dir, asset)
    try:
        try:
            download(f"{base}/sha256sum.txt", os.path.join(tmp_dir, "sha256sum.txt"))
            download(f"{base}/{asset}", tmp_bin)
        except Exception as e:  # noqa: BLE001 - 统一转成中文提示
            sys.exit(
                f"下载失败：{e}\n"
                "请检查网络；离线时可手动从 "
                f"https://github.com/{REPO}/releases 下载 {asset}，"
                f"放到 bin/{out_name}"
            )

        with open(os.path.join(tmp_dir, "sha256sum.txt"), encoding="utf-8") as f:
            want = expected_hash(f.read(), asset)
        got = sha256_of(tmp_bin)
        if got != want:
            sys.exit(f"sha256 校验失败（期望 {want}，实际 {got}），已删除下载文件")

        shutil.move(tmp_bin, out_path)
        if out_name == "ocr":
            os.chmod(out_path, os.stat(out_path).st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
        print(f"完成：{out_path}")
    finally:
        shutil.rmtree(tmp_dir, ignore_errors=True)


if __name__ == "__main__":
    main()
