#!/bin/bash
# 只读取公开打包名称。应用 identifier、可执行文件和用户配置目录保持稳定。
app_product_name() {
  node -e '
    const fs = require("node:fs");
    const name = JSON.parse(fs.readFileSync(process.argv[1], "utf8")).productName;
    if (typeof name !== "string" || !name.trim() || /[<>:"/\\|?*\x00-\x1f]/.test(name)) {
      throw new Error("tauri.conf.json 的 productName 必须是有效的跨平台文件名");
    }
    process.stdout.write(name);
  ' "$1"
}
