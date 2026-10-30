#!/usr/bin/env bash
# 在 Windows 本机（mingw-w64 / windows-gnu 工具链）构建 AcgDev 的 exe。
#
# 关键坑（与 README 互补）：
#   - GPUI 在 windows-gnu 下，mingw 会强制链入 default-manifest.o（一份空 RT_MANIFEST）。
#     若再直接把含 manifest 的 .rc 链入，GNU ld 无法合并两个 .rsrc 节，会报
#     ".rsrc merge failure" 并把全部资源丢弃，运行时即 “无法定位程序输入点
#     TaskDialogIndirect”。
#   - 因此本工程不在链接期嵌资源，而是构建后用 tools/embed_manifest.py 把
#     “图标 + 版本 + comctl32 v6 manifest” 一次性注入，并跑 tools/patch_icu_import.py
#     把 windows 0.61 写错的 icuuc.dll 导入改成系统实际的 icu.dll。
#
# 用法：
#   bash tools/build-win64.sh [debug|release]
#   release 需要 Windows SDK 的 fxc.exe（着色器离线预编译）；本机无 fxc 时用 debug。
set -euo pipefail

PROFILE="${1:-debug}"
TARGET="x86_64-pc-windows-gnu"
EXE="target/${TARGET}/${PROFILE}/acgdev.exe"

case "$PROFILE" in
  debug)   CARGO_PROFILE_DEV_DEBUG=0 cargo build --target "$TARGET" -p acgdev ;;
  release) cargo build --release --target "$TARGET" -p acgdev ;;
  *) echo "未知 profile: $PROFILE（用 debug 或 release）" >&2; exit 1 ;;
esac

echo ">>> 注入资源（图标 + 版本 + comctl32 v6 manifest）"
python3 tools/embed_manifest.py "$EXE"

echo ">>> 修补 icu 导入名（icuuc.dll -> icu.dll）"
python3 tools/patch_icu_import.py "$EXE"

echo ">>> 移除调试信息节（binutils/llvm strip 处理不了本工程 PE，用专用脚本）"
python3 tools/strip_debug.py "$EXE" --in-place

echo ">>> 完成：$EXE"
ls -la "$EXE"
