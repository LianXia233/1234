#!/usr/bin/env bash
# 交叉编译 Windows 版时，gpui 的 build.rs 会整段跳过着色器预编译：
# 它的 `mod windows` 用 `#[cfg(target_os = "windows")]` 判定，而 build.rs
# 是跑在 **host**（Linux）上的，条件永远为假，于是 OUT_DIR 里没有
# shaders_bytes.rs，随后 directx_renderer.rs 的 include! 直接失败。
#
# 这个脚本补上那一步：用 wine 跑 fxc2.exe（vkd3d 版 HLSL→DXBC 编译器），
# 按 build.rs 完全相同的模块表与命名规则生成 shaders_bytes.rs。
#
# 用法：先 cargo build 一次拿到 OUT_DIR（会失败），再运行本脚本，然后再 build。
#   ./tools/gen_gpui_shaders.sh [target-triple] [profile]
set -euo pipefail

TRIPLE="${1:-x86_64-pc-windows-gnu}"
PROFILE="${2:-release}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FXC2_EXE="${FXC2_EXE:-$ROOT/tools/fxc2.exe}"

GPUI_SRC="$(ls -d "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/gpui-0.2.2 2>/dev/null | head -1)"
[ -n "$GPUI_SRC" ] || { echo "找不到 gpui-0.2.2 源码目录" >&2; exit 1; }
[ -f "$FXC2_EXE" ] || { echo "找不到 fxc2.exe：$FXC2_EXE" >&2; exit 1; }

# 改动 gpui 的 feature 会改变 build 目录哈希，旧目录会残留；
# 取修改时间最新的那个，才是这次构建真正使用的 OUT_DIR。
BUILD_DIR="$(ls -dt "$ROOT"/target/"$TRIPLE"/"$PROFILE"/build/gpui-*/ 2>/dev/null | head -1)"
if [ -z "$BUILD_DIR" ]; then
    echo "找不到 target/$TRIPLE/$PROFILE/build/gpui-*/：先跑一次 cargo build（失败在 shaders_bytes.rs 那步即可），" >&2
    echo "让 cargo 执行 gpui 的 build.rs、把 OUT_DIR 建出来。" >&2
    exit 1
fi
OUT="$BUILD_DIR/out"
mkdir -p "$OUT"

export WINEPREFIX="${WINEPREFIX:-$HOME/.wine-fxc2}"
export WINEDEBUG="${WINEDEBUG:--all}"

SHADERS="$GPUI_SRC/src/platform/windows/shaders.hlsl"
EMOJI="$GPUI_SRC/src/platform/windows/color_text_raster.hlsl"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

DEST="$OUT/shaders_bytes.rs"
: > "$DEST"

# build.rs 里同样的顺序与命名
emit() { # $1=module $2=hlsl 路径
    local mod="$1" src="$2" stage entry var header
    for stage in vertex fragment; do
        if [ "$stage" = vertex ]; then
            entry="${mod}_vertex"; var="$(echo "$mod" | tr '[:lower:]' '[:upper:]')_VERTEX_BYTES"; tgt=vs_4_1
        else
            entry="${mod}_fragment"; var="$(echo "$mod" | tr '[:lower:]' '[:upper:]')_FRAGMENT_BYTES"; tgt=ps_4_1
        fi
        header="$TMP/${mod}_$stage.h"
        wine "$FXC2_EXE" /T "$tgt" /E "$entry" /Fh "$header" /Vn "$var" /O3 "$src" >/dev/null
        # 复刻 build.rs: 取 "const BYTE" 之后的 "= {...}" 并改成 Rust 切片字面量
        python3 - "$header" "$var" "$DEST" <<'PY'
import sys
hdr, name, dest = sys.argv[1], sys.argv[2], sys.argv[3]
src = open(hdr, encoding="utf-8", errors="replace").read()
body = src[src.index("const BYTE"):]
body = body[body.index("=") + 1:].strip()
body = body.replace("{", "[").replace("}", "]")
with open(dest, "a", encoding="utf-8") as f:
    f.write(f"const {name}: &[u8] = &{body}\n")
PY
    done
}

for m in quad shadow path_rasterization path_sprite underline monochrome_sprite polychrome_sprite; do
    emit "$m" "$SHADERS"
done
emit emoji_rasterization "$EMOJI"

echo "已生成 $DEST ($(wc -c < "$DEST") 字节, $(grep -c '^const ' "$DEST") 个着色器常量)"
