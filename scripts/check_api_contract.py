#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""检查 Rust 命令签名 / tauri-bridge.js 参数映射 / 前端调用是否一致。

Tauri 2 的 #[tauri::command] 宏默认把 Rust 的 snake_case 参数名转成
camelCase 作为 JS 侧键名，桥接层因此同时发送两种键。本脚本确保：
  1. 每个 Rust 命令都在桥接层登记；
  2. 桥接层的 snake_case 键与 Rust 参数一一对应；
  3. camelCase 转换结果与 Tauri 宏一致；
  4. 前端每个 pywebview.api.xxx(...) 调用的实参数量足够。

用法：python3 scripts/check_api_contract.py [--json-out /tmp/contract.json]
"""
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MAIN_RS = ROOT / "src-tauri" / "src" / "main.rs"
BRIDGE_JS = ROOT / "dist" / "tauri-bridge.js"
FRONTEND = [ROOT / "dist" / "app.js", ROOT / "dist" / "index.html"]

# 由 Tauri 注入、不参与 JS 键名的参数类型
INJECTED = ("AppHandle", "State<", "Window<", "Webview<", "App<", "Emitter<")


def to_camel(name):
    return re.sub(r"_([a-z])", lambda m: m.group(1).upper(), name)


def rust_commands():
    src = MAIN_RS.read_text(encoding="utf-8")
    pat = re.compile(
        r"#\[tauri::command[^\]]*\]\s*\n\s*fn\s+(\w+)\s*\(([^)]*)\)", re.S
    )
    out = {}
    for name, params in pat.findall(src):
        keys = []
        for raw in params.split(","):
            raw = raw.strip()
            if not raw or ":" not in raw:
                continue
            arg = raw.split(":", 1)[0].strip()
            typ = raw.split(":", 1)[1].strip()
            if any(t in typ for t in INJECTED):
                continue
            keys.append(arg)
        out[name] = keys
    return out


def bridge_args():
    src = BRIDGE_JS.read_text(encoding="utf-8")
    m = re.search(r"var ARG_NAMES = \{(.*?)\n    \};", src, re.S)
    if not m:
        raise SystemExit("未找到 ARG_NAMES 定义")
    out = {}
    for name, items in re.findall(r"(\w+)\s*:\s*\[([^\]]*)\]", m.group(1)):
        out[name] = [x.strip().strip("'\"") for x in items.split(",") if x.strip()]
    return out


def split_args(s):
    """按顶层逗号切分实参，忽略引号内与嵌套括号里的逗号。"""
    parts, cur, depth, quote = [], [], 0, None
    for ch in s:
        if quote:
            cur.append(ch)
            if ch == quote:
                quote = None
            continue
        if ch in "\"'":
            quote = ch
            cur.append(ch)
        elif ch in "([{":
            depth += 1
            cur.append(ch)
        elif ch in ")]}":
            depth -= 1
            cur.append(ch)
        elif ch == "," and depth == 0:
            parts.append("".join(cur).strip())
            cur = []
        else:
            cur.append(ch)
    if cur and "".join(cur).strip():
        parts.append("".join(cur).strip())
    return parts


def frontend_calls():
    out = {}
    for f in FRONTEND:
        for name, args in re.findall(
            r"pywebview\.api\.(\w+)\s*\(([^)]*)\)", f.read_text(encoding="utf-8")
        ):
            n = len(split_args(args)) if args.strip() else 0
            out.setdefault(name, []).append((f.name, n))
    return out


def main():
    rust, bridge, calls = rust_commands(), bridge_args(), frontend_calls()
    errors = []
    contract = {}

    for cmd, keys in sorted(rust.items()):
        camel = [to_camel(k) for k in keys]
        contract[cmd] = camel
        b = bridge.get(cmd)
        if b is None:
            errors.append("命令 %s 未在 tauri-bridge.js 的 ARG_NAMES 登记" % cmd)
            continue
        if b != keys:
            errors.append(
                "命令 %s 桥接键 %s 与 Rust 参数 %s 不一致" % (cmd, b, keys)
            )
        if [to_camel(k) for k in b] != camel:
            errors.append("命令 %s 的 camelCase 转换不一致" % cmd)
        for f, n in calls.get(cmd, []):
            if n < len(keys):
                errors.append(
                    "%s 中 pywebview.api.%s 只传了 %d 个参数，后端需要 %d 个"
                    % (f, cmd, n, len(keys))
                )

    for cmd in sorted(set(calls) - set(rust)):
        errors.append("前端调用了未实现的命令 %s" % cmd)

    print("Rust 命令 %d 个，桥接登记 %d 个，前端调用 %d 种"
          % (len(rust), len(bridge), len(calls)))
    print("camelCase 键示例：")
    for cmd in ("search", "add_dict_to_group", "advanced_search", "move_dict"):
        if cmd in contract:
            print("  %-22s %s" % (cmd, contract[cmd]))
    if errors:
        print("\n发现 %d 处不一致：" % len(errors))
        for e in errors:
            print("  ✖ " + e)
    else:
        print("\n契约检查通过：所有命令参数键与前端调用一致")

    out = next((a.split("=", 1)[1] for a in sys.argv[1:] if a.startswith("--json-out=")), None)
    if out:
        Path(out).write_text(json.dumps(contract, ensure_ascii=False, indent=2), encoding="utf-8")
        print("已写出 %s" % out)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
