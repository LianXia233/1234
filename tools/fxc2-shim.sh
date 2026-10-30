#!/bin/sh
# GPUI 在 Windows 上构建时会调用 fxc.exe 预编译 HLSL 着色器。
# 交叉编译（Linux → Windows）没有 fxc.exe，这里用 wine 跑 fxc2.exe
# ——它是 vkd3d-shader 封装出的 fxc 兼容 CLI，产出同样是标准 DXBC。
#
# 用法：
#   FXC2_EXE=/path/to/fxc2.exe \
#   GPUI_FXC_PATH=$PWD/tools/fxc2-shim.sh \
#   cargo build --release --target x86_64-pc-windows-gnu
set -e
: "${FXC2_EXE:?请设置 FXC2_EXE 指向 fxc2.exe}"
exec env \
    WINEPREFIX="${WINEPREFIX:-$HOME/.wine-fxc2}" \
    WINEDEBUG="${WINEDEBUG:--all}" \
    wine "$FXC2_EXE" "$@"
