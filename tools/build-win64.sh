#!/usr/bin/env bash
# 在 Windows 本机（mingw-w64 / windows-gnu 工具链）构建 AcgDev 的 exe。
#
# 关键坑（与 README 互补）：
#   - GPUI 在 windows-gnu 下，mingw 会强制链入 default-manifest.o（一份空 RT_MANIFEST）。
#     若再直接把含 manifest 的 .rc 链入，GNU ld 无法合并两个 .rsrc 节，会报
#     ".rsrc merge failure" 并把全部资源丢弃，运行时即 “无法定位程序输入点
#     TaskDialogIndirect”。
#   - 因此本工程链接期不嵌任何资源（build.rs 仅生成 app_res.o 作为备用，不链入），
#     构建后用 tools/fix_pe.py 做确定性的 PE 重写：
#       1) 移除调试节（/NNN、.debug_* 等），把 500+MB 的 debuginfo 压下来；
#       2) 用 windres 把 app.rc（图标 + 版本 + comctl32 v6 manifest）编成单一合法的
#          PE 资源目录，整体作为最后一个节追加，修正节表 / 资源数据目录 /
#          SizeOfImage / 校验和。
#     最后跑 tools/patch_icu_import.py 把 windows 0.61 写错的 icuuc.dll 导入名
#     改成系统实际的 icu.dll（否则下一个报错就是 icuuc.dll not found）。
#
# 用法：
#   bash tools/build-win64.sh [debug|release]
#   release 需要 Windows SDK 的 fxc.exe（着色器离线预编译）；本机无 fxc 时用 debug。
set -euo pipefail

PROFILE="${1:-debug}"
TARGET="x86_64-pc-windows-gnu"
EXE="target/${TARGET}/${PROFILE}/acgdev.exe"

case "$PROFILE" in
  # 注意：不要用 CARGO_PROFILE_DEV_DEBUG=0 —— 本机 rustc 在关闭 debuginfo 时崩溃；
  # debuginfo 由 fix_pe.py 在链接后裁掉即可。
  debug)   cargo build --target "$TARGET" -p acgdev ;;
  release) cargo build --release --target "$TARGET" -p acgdev ;;
  *) echo "未知 profile: $PROFILE（用 debug 或 release）" >&2; exit 1 ;;
esac

echo ">>> 重写 PE：移除调试节 + 注入图标/版本/comctl32 v6 manifest"
python3 tools/fix_pe.py "$EXE" crates/app/assets/app.rc

echo ">>> 修补 icu 导入名（icuuc.dll -> icu.dll）"
python3 tools/patch_icu_import.py "$EXE"

echo ">>> 完成：$EXE"
ls -la "$EXE"
