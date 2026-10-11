#!/bin/bash
# 用 Tauri 的**官方 JSON schema** 校验 tauri.conf.json。
#
# ## 为什么值得单独做一个检查
#
# 打包配置里的错误（键名拼错、枚举值写错）**在 macOS 上完全不会报错** ——
# Tauri 只在真正构建那个平台时才用到那些字段。所以等到 Windows 上
# `tauri build` 失败时，你已经在另一台机器上、面对一个陌生的报错。
#
# 实测踩过一次：往配置里加了个注释字段 `_targets说明`，
# 因为 schema 不允许未知字段，整个配置直接失效 —— macOS 构建也跟着挂了。
# 那次是在 macOS 上暴露的，纯属运气（坏的是通用段）。
#
# 用法：tools/check-tauri-config.sh
set -uo pipefail
cd "$(dirname "$0")/.."

CONF="apps/mixer-gui/tauri.conf.json"
SCHEMA_URL="https://schema.tauri.app/config/2"
CACHE_DIR="target/schema-cache"
SCHEMA="$CACHE_DIR/tauri-config-schema.json"
VENV="$CACHE_DIR/venv"

[ -f "$CONF" ] || { echo "  ❌ 找不到 $CONF"; exit 1; }

echo "── 校验 Tauri 打包配置 ──"

# ── schema：优先用缓存，没有就下 ──
mkdir -p "$CACHE_DIR"
if [ ! -s "$SCHEMA" ]; then
  echo "  下载 schema：$SCHEMA_URL"
  if ! curl -sL --max-time 30 "$SCHEMA_URL" -o "$SCHEMA" || [ ! -s "$SCHEMA" ]; then
    echo "  ⚠ 下不到 schema（网络问题？），跳过校验"
    exit 0
  fi
fi

# ── 校验器：优先用现成的 jsonschema，没有就建个临时 venv ──
PY=""
if python3 -c "import jsonschema" 2>/dev/null; then
  PY="python3"
elif [ -x "$VENV/bin/python" ]; then
  PY="$VENV/bin/python"
else
  echo "  建临时 venv 装 jsonschema（只做一次）…"
  if python3 -m venv "$VENV" >/dev/null 2>&1 && "$VENV/bin/pip" install --quiet jsonschema >/dev/null 2>&1; then
    PY="$VENV/bin/python"
  fi
fi

if [ -z "$PY" ]; then
  # 装不上也不该让整个检查失败 —— 退化成"结构自检"
  echo "  ⚠ 装不上 jsonschema，退化为结构自检"
  python3 - "$CONF" "$SCHEMA" <<'PY'
import json, sys
conf = json.load(open(sys.argv[1]))
schema = json.load(open(sys.argv[2]))
defs = schema.get("definitions", {})
props = set(defs.get("Config", {}).get("properties", {}))
bad = [k for k in conf if k not in props and k != "$schema"]
if bad:
    print(f"  ❌ 未知的顶层字段: {bad}")
    sys.exit(1)
print(f"  ✅ 顶层字段都是合法的（{len(conf)} 个）—— 但没做完整 schema 校验")
PY
  exit $?
fi

"$PY" - "$CONF" "$SCHEMA" <<'PY'
import json, sys, warnings
warnings.filterwarnings("ignore")
from jsonschema import Draft7Validator
try:
    from referencing import Registry, Resource
    from referencing.jsonschema import DRAFT7
    schema = json.load(open(sys.argv[2]))
    reg = Registry().with_resource(schema["$id"], Resource(contents=schema, specification=DRAFT7))
    v = Draft7Validator(schema, registry=reg)
except Exception:
    from jsonschema import RefResolver
    schema = json.load(open(sys.argv[2]))
    v = Draft7Validator(schema, resolver=RefResolver.from_schema(schema))
conf = json.load(open(sys.argv[1]))
errors = sorted(v.iter_errors(conf), key=lambda e: list(e.absolute_path))
if errors:
    print(f"  ❌ 配置不符合 Tauri schema，共 {len(errors)} 处：")
    for e in errors[:12]:
        path = ".".join(str(p) for p in e.absolute_path) or "(根)"
        print(f"     · {path}: {e.message[:160]}")
    sys.exit(1)
print("  ✅ tauri.conf.json 完全符合 Tauri 官方 schema")
PY
