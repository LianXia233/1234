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
set -euo pipefail
export MSYS_NO_PATHCONV=1

PROFILE="${1:-debug}"
TARGET="x86_64-pc-windows-gnu"
EXE="target/${TARGET}/${PROFILE}/acgdev.exe"

CC="${CC:-x86_64-w64-mingw32-gcc}"
WINDRES="${WINDRES:-windres}"

# 1) 定位 mingw 强制注入的 default-manifest.o（native 绝对路径）
SYS_DM="$("$CC" -print-file-name=default-manifest.o)"
if [ ! -f "$SYS_DM" ]; then
  echo "找不到 mingw 的 default-manifest.o：$SYS_DM" >&2
  exit 1
fi

# 2) 先把 app.manifest 转成 UTF-16LE（无 BOM），再用 windres 编成 COFF 资源对象。
#    关键点：Windows 的 SxS 激活上下文解析 RT_MANIFEST 时按 UTF-16LE 读取；直接把
#    UTF-8 的 app.manifest 交给 windres 会被原样拷贝进资源，Win11 便会报
#    “应用程序的并行配置不正确”。所以这里先转编码再编译。
#    图标 / 版本 / manifest 全部用 native 绝对路径，避免 cd 子 shell 里路径变相对。
REPO_WIN="$(cygpath -m "$PWD")"
OBJ="$REPO_WIN/target/.acgdev_manifest.o"
MAN_U16="$REPO_WIN/target/.acgdev_app.manifest.utf16"
RC_TMP="$REPO_WIN/target/.acgdev_app.rc"
ICON_WIN="$(cygpath -m "$PWD/crates/app/assets/icon.ico")"
MAN_SRC_WIN="$(cygpath -m "$PWD/crates/app/assets/app.manifest")"

python3 -c "open(r'$MAN_U16','wb').write(open(r'$MAN_SRC_WIN','rb').read().decode('utf-8').encode('utf-16-le'))"

cat > "$RC_TMP" <<RC
1 ICON "$ICON_WIN"

/* RT_MANIFEST = 24, ID 1. Requires comctl32 v6 for TaskDialogIndirect. */
1 24 "$MAN_U16"

1 VERSIONINFO
FILEVERSION     0,1,0,0
PRODUCTVERSION  0,1,0,0
FILEOS          0x40004L
FILETYPE        0x1L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "080404b0"
        BEGIN
            VALUE "CompanyName",      "AcgDev"
            VALUE "FileDescription",  "AcgDev HTTP Downloader"
            VALUE "FileVersion",      "0.1.0"
            VALUE "InternalName",     "acgdev"
            VALUE "LegalCopyright",   "MIT License"
            VALUE "OriginalFilename", "AcgDev.exe"
            VALUE "ProductName",      "AcgDev HTTP Downloader"
            VALUE "ProductVersion",   "0.1.0"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x804, 1200
    END
END
RC

"$WINDRES" -O coff "$RC_TMP" -o "$OBJ"

# 3) 备份并覆盖工具链的 default-manifest.o；trap 保证退出前恢复（含出错/中断）。
BAK="target/.acgdev_dm_backup.o"
cp "$SYS_DM" "$BAK"
restore() {
  cp "$BAK" "$SYS_DM" 2>/dev/null || true
  rm -f "$BAK" "$OBJ" "$MAN_U16" "$RC_TMP" 2>/dev/null || true
}
trap restore EXIT
cp "$OBJ" "$SYS_DM"

# 4) 工具链文件变了但项目没变，cargo 不会重链，强制重链。
rm -f "$EXE"

# 5) 构建（链接器 strip 去调试节）
case "$PROFILE" in
  debug)   cargo build --target "$TARGET" -p acgdev --config "profile.dev.strip=true" ;;
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

def u16(s):
    return s.encode("utf-16-le")

# SxS 要求 RT_MANIFEST 必须是 UTF-16LE；误存成 UTF-8 会让 Win11 报“并行配置不正确”。
asmv1 = u16("urn:schemas-microsoft-com:asm.v1")
ccv6  = u16("Microsoft.Windows.Common-Controls")
token = u16("6595b64144ccf175")
is_u16 = (asmv1 in d) and (ccv6 in d)
is_u8  = b"urn:schemas-microsoft-com:asm.v1" in d
print("manifest 按 UTF-16LE 嵌入:", is_u16)
print("manifest 误存成 UTF-8   :", is_u8)
print("comctl32 v6 token(utf16):", token in d)
print("icuuc.dll 残留:", b"icuuc.dll" in d, "| icu.dll 引用:", b"icu.dll" in d)
if not (d[:2] == b"MZ" and machine == 0x8664 and is_u16 and not is_u8
        and token in d and b"icuuc.dll" not in d):
    ok = False
print("RESULT:", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
PY
echo ">>> 完成：$EXE"
