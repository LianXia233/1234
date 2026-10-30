#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""把完整 Windows 资源（图标 + 版本 + comctl32 v6 manifest）注入到已链接好的 exe。

为什么需要它（windows-gnu 专属坑）
-----------------------------------
mingw 的 gcc spec 会对任何非共享可执行文件强制链入 default-manifest.o（一份空的
RT_MANIFEST）。若再直接把我们的 .rc（同样含 RT_MANIFEST id=1）链入，GNU ld 无法合并
两个 .rsrc 节，会报 ".rsrc merge failure" 并把全部资源整体丢弃，产物运行即
“无法定位程序输入点 TaskDialogIndirect”。

因此本工程的 .rc 不再被链接，而是在链接后用本脚本注入：
  1. 用 windres 把 crates/app/assets/app.rc（图标+版本+manifest）编译成单一 .o；
  2. 抽取该 .o 中合法的 .rsrc 节（资源目录偏移均为“相对 .rsrc 起点”，可整体搬运）；
  3. 把它整体写入 exe，重指 .rsrc 节头与资源数据目录，并重算 PE 校验和。

该方式对 MSVC / windows-gnu / 交叉编译一致有效，且不会与默认 manifest 冲突。

用法
----
    python3 tools/embed_manifest.py <exe> [app.rc]
"""
from __future__ import annotations

import os
import shutil
import struct
import subprocess
import sys
import tempfile


def align_up(x: int, a: int) -> int:
    return (x + (a - 1)) & ~(a - 1)


# ---------- PE / COFF 解析 ----------

def parse_pe(data: bytearray):
    if data[:2] != b"MZ":
        raise SystemExit("不是 PE（缺 MZ 头）")
    e_lfanew = struct.unpack_from("<I", data, 0x3C)[0]
    if data[e_lfanew:e_lfanew + 4] != b"PE\x00\x00":
        raise SystemExit("不是 PE（缺 PE 签名）")
    coff = e_lfanew + 4
    num_sections = struct.unpack_from("<H", data, coff + 2)[0]
    opt_size = struct.unpack_from("<H", data, coff + 16)[0]
    opt = coff + 20
    magic = struct.unpack_from("<H", data, opt)[0]
    if magic == 0x20B:
        dd = opt + 112            # PE32+
    elif magic == 0x10B:
        dd = opt + 96             # PE32
    else:
        raise SystemExit(f"未知可选头 magic: {magic:#x}")
    file_align = struct.unpack_from("<I", data, opt + 60)[0] or 0x200
    sec_align = struct.unpack_from("<I", data, opt + 56)[0] or 0x1000
    sections = []
    sec = opt + opt_size
    for i in range(num_sections):
        base = sec + i * 40
        name = data[base:base + 8].split(b"\x00", 1)[0].decode("ascii", "replace")
        vsize, vaddr, rawsize, rawptr = struct.unpack_from("<IIII", data, base + 8)
        sections.append(dict(name=name, vaddr=vaddr, vsize=vsize,
                             rawptr=rawptr, rawsize=rawsize, base=base))
    return dict(coff=coff, opt=opt, opt_size=opt_size, magic=magic, dd=dd,
                file_align=file_align, sec_align=sec_align,
                sections=sections, num_sections=num_sections,
                sec_table_off=sec, e_lfanew=e_lfanew)


def find_section(pe, name: str):
    for s in pe["sections"]:
        if s["name"] == name:
            return s
    return None


def extract_rsrc_from_obj(obj_path: str) -> bytes:
    """从 windres 生成的 .o（COFF/PE 对象）中抽取 .rsrc 节原始字节。"""
    data = open(obj_path, "rb").read()
    if data[:2] == b"MZ":
        # 已是 PE（不太可能，windres -O coff 产出 COFF）
        raise SystemExit("windres 产物不是预期的 COFF 对象")
    # COFF：e_lfanew 不存在，直接按 COFF 头解析
    # COFF 头：机器(2) 节数(2) 时间戳(4) 符号表(4) 符号数(4) 可选头大小(2) 特性(2)
    num_sections = struct.unpack_from("<H", data, 2)[0]
    opt_size = struct.unpack_from("<H", data, 16)[0]
    sec = 20 + opt_size
    for i in range(num_sections):
        base = sec + i * 40
        name = data[base:base + 8].split(b"\x00", 1)[0].decode("ascii", "replace")
        if name != ".rsrc":
            continue
        vsize, vaddr, rawsize, rawptr = struct.unpack_from("<IIII", data, base + 8)
        return data[rawptr:rawptr + rawsize]
    raise SystemExit("windres 产物中未找到 .rsrc 节")


# ---------- 校验和 ----------

def pe_checksum(data: bytearray) -> int:
    e_lfanew = struct.unpack_from("<I", data, 0x3C)[0]
    cs_off = e_lfanew + 4 + 20 + 64   # COFF(4) + 可选头(20) + CheckSum 域(64)
    if len(data) & 1:
        data.append(0)
    s = 0
    for i in range(0, len(data), 2):
        val = 0 if i == cs_off else struct.unpack_from("<H", data, i)[0]
        s += val
        if s > 0xFFFF:
            s = (s & 0xFFFF) + 1
    s += len(data)
    return s & 0xFFFFFFFF


# ---------- 注入 ----------

def inject_rsrc(exe_path: str, rsrc: bytes) -> None:
    data = bytearray(open(exe_path, "rb").read())
    pe = parse_pe(data)

    last = max(pe["sections"], key=lambda s: s["rawptr"] + s["rawsize"])
    new_raw = align_up(last["rawptr"] + last["rawsize"], pe["file_align"])
    # 节虚拟地址必须落在最后一个节之后并按 SectionAlignment 对齐
    last_va_end = last["vaddr"] + align_up(max(last["vsize"], last["rawsize"]), pe["sec_align"])
    new_va = align_up(last_va_end, pe["sec_align"])

    fa = pe["file_align"]
    raw_size = align_up(len(rsrc), fa)          # 文件对齐后的原始大小
    # 写数据到文件末尾
    if new_raw > len(data):
        data.extend(b"\x00" * (new_raw - len(data)))
    data[new_raw:new_raw + len(rsrc)] = rsrc
    if len(data) < new_raw + raw_size:
        data.extend(b"\x00" * (new_raw + raw_size - len(data)))

    # 重指现有 .rsrc 节头（不新增节，避免改动节表/头字段）
    rsrc_sec = find_section(pe, ".rsrc")
    if rsrc_sec is None:
        raise SystemExit("exe 中找不到 .rsrc 节")
    b = rsrc_sec["base"]
    struct.pack_into("<I", data, b + 8, len(rsrc))     # VirtualSize
    struct.pack_into("<I", data, b + 12, new_va)       # VirtualAddress
    struct.pack_into("<I", data, b + 16, raw_size)     # SizeOfRawData
    struct.pack_into("<I", data, b + 20, new_raw)      # PointerToRawData

    # 重指资源数据目录（IMAGE_DIRECTORY_ENTRY_RESOURCE = 2，位于可选头数据目录第 3 项）
    struct.pack_into("<II", data, pe["dd"] + 2 * 8, new_va, len(rsrc))

    # 重算校验和
    cs = pe_checksum(data)
    struct.pack_into("<I", data, pe["e_lfanew"] + 4 + 20 + 64, cs)

    open(exe_path, "wb").write(data)


def find_windres() -> str:
    target = os.environ.get("TARGET", "")
    candidates = [
        f"{target}-windres",
        "windres",
        "x86_64-w64-mingw32-windres",
        "i686-w64-mingw32-windres",
    ]
    for c in candidates:
        if shutil.which(c):
            return c
    raise SystemExit("找不到 windres（请安装 mingw-w64 工具链）")


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    exe_path = sys.argv[1]
    rc_path = sys.argv[2] if len(sys.argv) > 2 else \
        os.path.join("crates", "app", "assets", "app.rc")
    if not os.path.isfile(exe_path):
        print(f"exe 不存在: {exe_path}")
        return 1
    if not os.path.isfile(rc_path):
        print(f"rc 不存在: {rc_path}")
        return 1

    windres = find_windres()
    rc_dir = os.path.dirname(os.path.abspath(rc_path))
    # .rc 内用 "assets/icon.ico" 这类相对路径，基于 assets 的“父目录”解析，
    # 故 windres 的工作目录须设为 assets 的父目录（即 crates/app/）。
    rc_parent = os.path.dirname(rc_dir)
    with tempfile.NamedTemporaryFile(suffix=".o", delete=False) as tf:
        obj_path = tf.name
    try:
        # cwd 设为 assets 的父目录，rc 用绝对路径传入，保证 #include 与相对资源可解析
        r = subprocess.run([windres, "-O", "coff", "-I", rc_dir,
                           os.path.abspath(rc_path), "-o", obj_path],
                          cwd=rc_parent, capture_output=True, text=True)
        if r.returncode != 0:
            print("windres 失败:\n", r.stderr or r.stdout)
            return 1
        rsrc = extract_rsrc_from_obj(obj_path)
    finally:
        try:
            os.remove(obj_path)
        except OSError:
            pass

    if b"Microsoft.Windows.Common-Controls" not in rsrc or b"6.0.0.0" not in rsrc:
        print("警告：生成的资源未含 comctl32 v6 声明，TaskDialogIndirect 仍会失败。")
    inject_rsrc(exe_path, rsrc)
    print(f"已注入完整资源（{len(rsrc)} 字节）到 {exe_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
