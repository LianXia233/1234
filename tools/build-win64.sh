#!/usr/bin/env bash
# 在 Windows 本机（mingw-w64 / windows-gnu 工具链）构建 AcgDev 的 exe。
#
# 为什么用“覆盖 default-manifest.o”而不是直接链 .rc：
#   mingw 的 gcc 会强制链入工具链自带的 default-manifest.o（一份不含任何依赖的
#   空 RT_MANIFEST）。若再把含 manifest 的 .rc 直接链入，GNU ld 无法合并两个
#   .rsrc 节，会报 “.rsrc merge failure” 并把全部资源丢弃；而事后手工重写 PE 又会
#   把 PE 结构弄坏（Windows 加载器直接拒载，“此应用无法在你的电脑上运行”）。
#   因此本脚本在构建期临时用“图标+版本+comctl32 v6 manifest”的资源对象覆盖工具链
#   的 default-manifest.o，让链接器*原生*产出带正确 manifest 的合法 PE；构建结束后
#   立即恢复原文件（trap 保证任何退出路径都会恢复，避免污染全局工具链）。
#
# 调试节（/NNN、.debug_* 等，会让 exe 膨胀到 500+MB）由本机链接器的 strip 在链接期
# 直接去掉（cargo 的 profile.<p>.strip=true），而不是事后用 strip 工具——binutils /
# llvm 的 strip 都处理不了本工程这种含大量 /NNN 调试节的 Rust PE。
#
# 最后跑 tools/patch_icu_import.py：windows 0.61 会在导入表里把 DLL 名写成
# icuuc.dll，而 Windows 系统只有 icu.dll，必须改成 icu.dll，否则下一个报错就是
# “icuuc.dll not found”。
#
# 用法：
#   bash tools/build-win64.sh [debug|release]
#   release 需要 Windows SDK 的 fxc.exe（GPUI 着色器离线预编译）；本机无 fxc 用 debug。
#
# 两种宿主环境都支持：
#   · Windows（MSYS2 / Git Bash + WinLibs）：windres 是 Windows 原生程序，路径要
#     用 cygpath 转成 W:\ 形式；对象文件名也相应写成反斜杠路径。
#   · Linux（CI 交叉编译）：windres 是原生 ELF，直接用 POSIX 路径即可，且工具链
#     文件在 /usr 下，需要对 default-manifest.o 有写权限（CI 里提前 chmod）。
# 两种环境下链接产物完全一致，因为资源内容与链接方式不变。
set -euo pipefail
export MSYS_NO_PATHCONV=1

PROFILE="${1:-debug}"
TARGET="x86_64-pc-windows-gnu"
EXE="target/${TARGET}/${PROFILE}/acgdev.exe"

CC="${CC:-x86_64-w64-mingw32-gcc}"
# windres 的名字随发行版不同：Debian/Ubuntu 带完整 target 前缀，MSYS2/WinLibs 就叫 windres。
if [ -z "${WINDRES:-}" ]; then
  if command -v x86_64-w64-mingw32-windres >/dev/null 2>&1; then
    WINDRES="x86_64-w64-mingw32-windres"
  elif command -v windres >/dev/null 2>&1; then
    WINDRES="windres"
  else
    echo "找不到 windres（需要 mingw-w64 的 x86_64-w64-mingw32-windres）" >&2
    exit 1
  fi
fi

# 1) 定位 mingw 强制注入的 default-manifest.o（native 绝对路径）
SYS_DM="$("$CC" -print-file-name=default-manifest.o)"
if [ ! -f "$SYS_DM" ]; then
  echo "找不到 mingw 的 default-manifest.o：$SYS_DM" >&2
  exit 1
fi

# 2) 用完整 app.rc（图标 + 版本 + comctl32 v6 manifest）编成 COFF 资源对象。
#    windres 以 crates/app 为工作目录编译 assets/app.rc，使 assets/icon.ico 等
#    相对路径可正确解析。输出写成 native 绝对路径，避免在 cd 子 shell 里变相对。
#
#    关于 manifest 编码：保持 UTF-8 原样嵌入即可。事件日志实测（SideBySide 事件）
#    证明 Windows SxS 能正常解析 UTF-8 的 RT_MANIFEST；反而改成无 BOM 的 UTF-16LE
#    会因 XML 声明写着 encoding="UTF-8" 而被解析器按 UTF-8 读取，报“无效的 Xml 语法”。
#    真正的关键点是 publicKeyToken 必须是 6595b64144ccf1df（WinSxS 实测值），
#    写成 6595b64144ccf175 会导致“找不到从属程序集”。
mkdir -p target
if command -v cygpath >/dev/null 2>&1; then
  # MSYS：windres 是 Windows 原生程序，必须给它 Windows 形式的路径。
  OBJ="$(cygpath -w "$PWD")\\target\\.acgdev_manifest.o"
else
  # Linux：windres 是原生 ELF，POSIX 路径即可。
  OBJ="$PWD/target/.acgdev_manifest.o"
fi
( cd crates/app && "$WINDRES" assets/app.rc -O coff -o "$OBJ" )

# 3) 备份并覆盖工具链的 default-manifest.o；trap 保证退出前恢复（含出错/中断）。
if [ ! -w "$SYS_DM" ]; then
  echo "对 $SYS_DM 没有写权限，无法临时覆盖 default-manifest.o。" >&2
  echo "Linux/CI 上请先执行：" >&2
  echo "  sudo chmod a+w \"\$($CC -print-file-name=default-manifest.o)\"" >&2
  exit 1
fi
BAK="target/.acgdev_dm_backup.o"
cp "$SYS_DM" "$BAK"
restore() {
  cp "$BAK" "$SYS_DM" 2>/dev/null || true
  rm -f "$BAK" "$OBJ" 2>/dev/null || true
}
trap restore EXIT
cp "$OBJ" "$SYS_DM"

# 4) 工具链文件变了但项目没变，cargo 不会重链，强制重链。
rm -f "$EXE"

# 5) 构建（链接器 strip 去调试节）
#
#    FAST_DEBUG=1 时把本项目两个 crate 提到 O3（第三方依赖本来就是 opt-level=2）。
#    默认关闭，以保证开发期增量编译速度；云端出可供日常使用的 exe 时打开。
case "$PROFILE" in
  debug)
    EXTRA=()
    if [ "${FAST_DEBUG:-0}" = "1" ]; then
      EXTRA=(--config 'profile.dev.package.acgdev-core.opt-level=3'
             --config 'profile.dev.package.acgdev.opt-level=3')
    fi
    cargo build --target "$TARGET" -p acgdev --config "profile.dev.strip=true" "${EXTRA[@]}"
    ;;
  release) cargo build --release --target "$TARGET" -p acgdev --config "profile.release.strip=true" ;;
  *) echo "未知 profile：$PROFILE（用 debug 或 release）" >&2; exit 1 ;;
esac

# 6) 修补 icu 导入名（icuuc.dll -> icu.dll）
echo ">>> 修补 icu 导入名（icuuc.dll -> icu.dll）"
python3 tools/patch_icu_import.py "$EXE"

# 7) 校验
echo ">>> 校验产物"
EXE="$EXE" python3 - <<'PY'
import os, struct, sys
exe = os.environ.get("EXE", "target/x86_64-pc-windows-gnu/debug/acgdev.exe")
d = open(exe, "rb").read()
ok = True
print("大小( MB ):", round(len(d) / 1048576, 1))
print("MZ 头:", "OK" if d[:2] == b"MZ" else "BAD")
e = struct.unpack_from("<I", d, 0x3C)[0]
machine = struct.unpack_from("<H", d, e + 4)[0]
print("机器码 x64(0x8664):", machine == 0x8664)

# manifest 以 UTF-8 嵌入（本机 SxS 实测可解析）；token 必须是 WinSxS 里的真实值。
asmv1 = b"urn:schemas-microsoft-com:asm.v1"
ccv6  = b"Microsoft.Windows.Common-Controls"
token = b"6595b64144ccf1df"   # comctl32 v6 真实 publicKeyToken
bad   = b"6595b64144ccf175"   # 错误值：会被报“找不到从属程序集”
print("manifest 以 UTF-8 嵌入:", (asmv1 in d) and (ccv6 in d))
print("comctl32 v6 token 正确:", token in d)
print("错误 token 残留       :", bad in d)
print("icuuc.dll 残留:", b"icuuc.dll" in d, "| icu.dll 引用:", b"icu.dll" in d)
if not (d[:2] == b"MZ" and machine == 0x8664 and asmv1 in d and ccv6 in d
        and token in d and bad not in d and b"icuuc.dll" not in d):
    ok = False
print("RESULT:", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
PY
echo ">>> 完成：$EXE"

# 8) 拷一份到 dist/，文件名带版本号与 profile，便于 CI 直接上传 artifact
VER="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
[ -n "$VER" ] || VER="0.1.0"
SUFFIX=""
[ "$PROFILE" = "debug" ] && SUFFIX="-debug"
DIST="dist/AcgDev-${VER}-win64${SUFFIX}.exe"
mkdir -p dist
cp "$EXE" "$DIST"
echo ">>> 产物：$DIST"
