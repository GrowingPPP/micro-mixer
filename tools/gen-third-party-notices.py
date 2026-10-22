#!/usr/bin/env python3
"""生成 THIRD-PARTY-NOTICES.md —— 随产品分发的第三方依赖声明。

为什么需要：MIT / Apache-2.0 / BSD 这类许可证都要求**保留版权声明**。
发布二进制时带上这份声明才算合规。

    python3 tools/gen-third-party-notices.py

**收录范围**：**只算 macOS 目标**（arm64 + x86_64 取并集）下，
从 workspace 成员出发沿 `normal` 与 `build` 依赖可达的 crate。

不加平台过滤的话会把 Windows / Android / Linux 专用的依赖也算进来
（实测 549 个 vs 实际 300 出头），声明里列一堆根本不随包分发的东西，
既没意义又掩盖了真正需要关注的条目。

**排除** `dev` 依赖（测试专用，不进产物），也排除 workspace 自己的成员。

> 为什么连 `build` 依赖也收：严格说构建脚本和过程宏的代码不进二进制，
> 但它们的**输出**可能被链接进去（比如 build script 编译的 C 代码），
> 而且多列几条没有坏处 —— 漏列才有风险。

许可证原文直接从本地 cargo registry 里各 crate 自带的 LICENSE 文件读，
不是从网上抄的 —— 这样和实际随包分发的内容一致。
"""

from __future__ import annotations

import json
import os
import pathlib
import re
import subprocess
import sys
from collections import defaultdict

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "THIRD-PARTY-NOTICES.md"


#: 产品实际发布的平台。取并集是因为要出通用二进制（arm64 + x86_64）。
TARGETS = ["aarch64-apple-darwin", "x86_64-apple-darwin"]


def cargo_metadata(target: str | None = None) -> dict:
    cmd = ["cargo", "metadata", "--format-version", "1"]
    if target:
        cmd += ["--filter-platform", target]
    out = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True, check=True).stdout
    return json.loads(out)


def shipped_packages(meta: dict) -> list[dict]:
    """从 workspace 成员出发，沿 normal + build 依赖走一遍。"""
    by_id = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    members = set(meta["workspace_members"])

    seen: set[str] = set()
    stack = list(members)
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.add(pid)
        node = nodes.get(pid)
        if not node:
            continue
        for dep in node["deps"]:
            kinds = {k["kind"] for k in dep.get("dep_kinds", [])}
            # None 表示普通依赖；"build" 是构建依赖；"dev" 是测试专用 —— 排除
            if kinds and kinds <= {"dev"}:
                continue
            stack.append(dep["pkg"])

    return sorted(
        (by_id[p] for p in seen - members if p in by_id),
        key=lambda p: p["name"].lower(),
    )


def find_license_files(pkg: dict) -> list[tuple[str, str]]:
    """在 registry 源码目录里找这个 crate 自带的许可证文件。"""
    src = pkg.get("manifest_path")
    if not src:
        return []
    d = pathlib.Path(src).parent
    if not d.exists():
        return []
    found = []
    for f in sorted(d.iterdir()):
        if not f.is_file():
            continue
        n = f.name.upper()
        if n.startswith(("LICENSE", "LICENCE", "COPYING", "UNLICENSE", "NOTICE")):
            try:
                text = f.read_text(errors="replace").strip()
            except OSError:
                continue
            if text:
                found.append((f.name, text))
    return found


#: 从原文正文认出这是哪份标准许可证。
#: 用于给「Cargo.toml 声明了标识、但包内没带 LICENSE 文件」的 crate 做回退 ——
#: 这类 crate 不少（实测 32/303），只在 Cargo.toml 里写 `license = "MIT"`，
#: 发布的 .crate 里没有原文。光列个标识不足以满足"保留版权声明"的要求。
_SIGNATURES = [
    ("MPL-2.0", ["Mozilla Public License Version 2.0", "Mozilla Public License, v. 2.0"]),
    ("Apache-2.0", ["Apache License", "Version 2.0, January 2004"]),
    ("MIT", ["Permission is hereby granted, free of charge"]),
    ("BSD-3-Clause", ["Redistribution and use in source and binary forms",
                      "Neither the name of"]),
    ("BSD-2-Clause", ["Redistribution and use in source and binary forms"]),
    ("ISC", ["Permission to use, copy, modify, and/or distribute this software"]),
    ("Zlib", ["This software is provided 'as-is'", "altered source versions"]),
    ("Unicode-3.0", ["UNICODE LICENSE V3", "Unicode License V3"]),
    ("Unlicense", ["This is free and unencumbered software released into the public domain"]),
    ("0BSD", ["Zero-Clause BSD", "Permission to use, copy, modify, and/or distribute"]),
    ("CC0-1.0", ["CC0 1.0 Universal"]),
]


def classify(text: str) -> str | None:
    for ident, needles in _SIGNATURES:
        if all(n in text for n in needles):
            return ident
    return None


def heading_for(text: str) -> str:
    """给一份许可证原文起个可读的标题。

    优先用**识别出的许可证标识**（`MIT` / `Apache-2.0` / `MPL-2.0`…）——
    直接取首行的话，很多原文开头是分隔线或版权人，标题会变成
    `========` 或者 `(C) 2024 Trifecta Tech Foundation`，没法看。
    """
    ident = classify(text)
    if ident:
        return ident
    for i, line in enumerate(text.splitlines()):
        line = line.strip().strip("/*#-= ").strip()
        # 开头几行如果是 `键: 值` 形式的机器元数据（SPDX 头就是这种），跳过 ——
        # 那不是给人看的标题，取到会变成 "DataLicense: CC0-1.0" 这种。
        if i < 24 and re.match(r"^[A-Za-z][A-Za-z0-9]*:\s", line):
            continue
        if line.upper().startswith(("SPDX", "CREATED:", "CREATOR:", "UUID:")):
            continue
        if len(line) >= 4:
            return line[:70]
    return "（无标题）"


def resolve_expression(expr: str, available: dict[str, str]) -> str | None:
    """把 `MIT OR Apache-2.0` 这类表达式映射到一份可用的原文。

    双许可意味着使用方可以任选其一，所以给出其中任意一份都算合规。
    """
    if not expr or expr == "(未声明)":
        return None
    for part in re.split(r"\s+(?:OR|AND)\s+|/", expr):
        ident = part.strip().strip("()")
        # 去掉 `WITH LLVM-exception` 之类的后缀
        ident = re.sub(r"\s+WITH\s+.*$", "", ident).strip()
        if ident in available:
            return available[ident]
    return None


def main() -> int:
    # 两个 macOS 目标各解一遍再合并 —— 有些依赖是分架构的
    merged: dict[str, dict] = {}
    for t in TARGETS:
        for p in shipped_packages(cargo_metadata(t)):
            merged[p["id"]] = p
    pkgs = sorted(merged.values(), key=lambda p: p["name"].lower())
    print(f"  收录 {len(pkgs)} 个第三方 crate（macOS 目标：{' + '.join(TARGETS)}）")

    by_license: dict[str, list[str]] = defaultdict(list)
    texts: dict[str, tuple[str, list[str]]] = {}  # 去重后的许可证原文
    missing: list[str] = []

    for p in pkgs:
        lic = p.get("license") or "(未声明)"
        by_license[lic].append(f"{p['name']} {p['version']}")
        files = find_license_files(p)
        if not files:
            missing.append(f"{p['name']} {p['version']}")
            continue
        for name, text in files:
            key = re.sub(r"\s+", " ", text)[:400]  # 按内容去重
            if key not in texts:
                texts[key] = (heading_for(text), [text, []])
            texts[key][1][1].append(f"{p['name']} {p['version']}")

    # ── 回退：给包内没带 LICENSE 的 crate，按声明的标识补标准原文 ──
    canonical: dict[str, str] = {}
    for _key, (name, (text, _users)) in texts.items():
        ident = classify(text)
        if ident and ident not in canonical:
            canonical[ident] = text

    still_missing: list[str] = []
    fallback_used = 0
    for entry in missing:
        crate, _, _ver = entry.partition(" ")
        expr = next(
            (p.get("license") or "" for p in pkgs if p["name"] == crate), ""
        )
        text = resolve_expression(expr, canonical)
        if not text:
            still_missing.append(f"{entry}（声明：{expr or '无'}）")
            continue
        key = re.sub(r"\s+", " ", text)[:400]
        if key not in texts:
            texts[key] = (f"{heading_for(text)}（标准原文）", [text, []])
        texts[key][1][1].append(entry)
        fallback_used += 1
    print(f"  {fallback_used} 个 crate 用声明的标识回退到标准原文")

    # ── 输出 ──
    L: list[str] = []
    L.append("# 第三方依赖声明\n")
    L.append(
        "本产品本身以 **MIT** 许可证发布（见 [LICENSE](LICENSE)）。"
        "它包含以下第三方开源软件，各软件的版权归其各自作者所有，"
        "按各自的许可证条款使用。\n"
    )
    L.append(
        "> 本文件由 `tools/gen-third-party-notices.py` 自动生成，请勿手工编辑。\n"
        "> 重新生成：`python3 tools/gen-third-party-notices.py`\n"
    )
    L.append(f"共收录 **{len(pkgs)}** 个第三方 crate（Rust）。\n")
    L.append(
        f"平台：{' + '.join(TARGETS)}（不含其他平台专用的依赖）。\n"
        "收录范围：从 workspace 成员出发，沿**普通依赖与构建依赖**可达的全部 crate；\n"
        "排除测试专用依赖。许可证原文取自各 crate 随包分发的 LICENSE 文件。\n"
    )

    L.append("\n---\n\n## 依赖清单（按许可证分组）\n")
    for lic in sorted(by_license, key=lambda k: (-len(by_license[k]), k)):
        names = by_license[lic]
        L.append(f"\n### {lic} —— {len(names)} 个\n")
        for n in sorted(names, key=str.lower):
            L.append(f"- {n}")

    if still_missing:
        L.append("\n---\n\n## ⚠️ 仍未能确定许可证原文的 crate\n")
        L.append(
            "下面这些 crate 的包内没有 LICENSE 文件，且无法从声明的标识"
            "匹配到已知的标准许可证原文。**发布前需要人工核对**：\n"
        )
        for n in still_missing:
            L.append(f"- {n}")

    L.append("\n---\n\n## 许可证全文\n")
    L.append(
        "按**内容**去重：正文完全相同的只列一份，并注明它覆盖了哪些 crate。\n"
        "注意不同 crate 的 MIT/Apache 正文里版权人不同，所以看起来相似的条文会有多份 —— "
        "这是对的，每个版权声明都要保留。\n"
    )
    # 标题相同的多份原文（不同版权人）按覆盖的 crate 列表决定顺序。
    # 否则相同标题会退回到字典插入顺序，而该顺序取决于跨进程不稳定的集合遍历，
    # 连续生成会出现内容相同但顺序不同的差异。
    for i, (_key, (title, (text, users))) in enumerate(
        sorted(
            texts.items(),
            key=lambda kv: (kv[1][0].lower(), tuple(sorted(set(kv[1][1][1])))),
        ),
        start=1,
    ):
        L.append(f"\n### {i}. {title}\n")
        crates = users
        shown = ", ".join(sorted(set(crates))[:8])
        more = f" 等 {len(set(crates))} 个" if len(set(crates)) > 8 else ""
        L.append(f"覆盖 {len(set(crates))} 个 crate：{shown}{more}\n")
        L.append("\n```text\n" + text.rstrip() + "\n```\n")

    OUT.write_text("\n".join(L) + "\n")
    size = OUT.stat().st_size
    print(f"  已写入 {OUT.relative_to(ROOT)}（{size/1024:.0f} KB，{len(texts)} 份许可证原文）")
    if still_missing:
        print(f"  ⚠ {len(still_missing)} 个 crate 无法确定许可证原文，见文件末尾")
    else:
        print("  ✅ 全部 crate 都能给出许可证原文")
    return 0


if __name__ == "__main__":
    sys.exit(main())
