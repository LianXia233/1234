#!/usr/bin/env bash
# 一键构建 Windows x64 单文件 exe（在 Linux 上交叉编译）。
#
#   ./tools/build-windows.sh
#
# 依赖（Debian/Ubuntu）：
#   rustup target add x86_64-pc-windows-gnu
#   apt install gcc-mingw-w64-x86-64 binutils-mingw-w64-x86-64 wine64
#   tools/fxc2.exe —— vkd3d 版 HLSL→DXBC 编译器（见 README「Windows 构建」）
#
# 产物：dist/AcgDev-<version>-win64.exe
set -euo pipefail

TRIPLE=x86_64-pc-windows-gnu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
DIST="$ROOT/dist"
EXE="$ROOT/target/$TRIPLE/release/acgdev.exe"

export FXC2_EXE="${FXC2_EXE:-$ROOT/tools/fxc2.exe}"
export WINEPREFIX="${WINEPREFIX:-$HOME/.wine-fxc2}"
export WINEDEBUG="${WINEDEBUG:--all}"

[ -f "$FXC2_EXE" ] || { echo "缺少 $FXC2_EXE（HLSL 编译器，见 README）" >&2; exit 1; }
command -v wine >/dev/null || { echo "缺少 wine" >&2; exit 1; }
command -v "$TRIPLE-gcc" >/dev/null || command -v x86_64-w64-mingw32-gcc >/dev/null \
    || { echo "缺少 x86_64-w64-mingw32-gcc（mingw-w64）" >&2; exit 1; }

echo "==> 1/4 首次构建（gpui 的着色器产物此时还不存在，预期在这一步失败）"
cargo build --release --target "$TRIPLE" -p acgdev || true

echo "==> 2/4 生成 gpui 的 DXBC 着色器字节码"
"$ROOT/tools/gen_gpui_shaders.sh" "$TRIPLE" release

echo "==> 3/4 正式构建"
cargo build --release --target "$TRIPLE" -p acgdev

echo "==> 4/4 修正 icu 导入名并打包"
python3 "$ROOT/tools/patch_icu_import.py" "$EXE"
mkdir -p "$DIST"
OUT="$DIST/AcgDev-$VERSION-win64.exe"
cp "$EXE" "$OUT"

SIZE=$(stat -c%s "$OUT")
SHA=$(sha256sum "$OUT" | cut -d' ' -f1)
echo
echo "产物: $OUT"
echo "大小: $SIZE 字节 ($((SIZE / 1024 / 1024)) MiB)"
echo "SHA256: $SHA"
