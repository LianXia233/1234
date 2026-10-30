#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""移除 Rust/mingw 生成的 PE 中的调试信息节，缩小 exe 体积。

binutils strip / llvm-strip 都处理不了本工程这种含大量 /NNN 调试贡献节、节号
超范围的 PE，故用本脚本直接重写文件：丢弃 .debug_* 与 /NNN 等调试节，保留
代码/数据/资源节（含我们注入的 .rsrc），并重算节表、SizeOfImage 与校验和。

用法: python3 tools/strip_debug.py <exe> [--in-place]
"""
from __future__ import annotations
import struct
import sys

DEBUG_NAME_PREFIXES = ("/", ".debug", ".zdebug", ".rdata$")
DEBUG_NAME_EXACT = set()  # 如需额外剔除可在此加


def is_debug_section(name: str) -> bool:
    if name in DEBUG_NAME_EXACT:
        return True
    for p in DEBUG_NAME_PREFIXES:
        if name.startswith(p):
            return True
    return False


def pe_checksum(data: bytearray) -> int:
    e_lfanew = struct.unpack_from("<I", data, 0x3C)[0]
    cs_off = e_lfanew + 4 + 20 + 64
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


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    path = sys.argv[1]
    inplace = "--in-place" in sys.argv[2:]
    data = bytearray(open(path, "rb").read())

    e_lfanew = struct.unpack_from("<I", data, 0x3C)[0]
    coff = e_lfanew + 4
    num_sections = struct.unpack_from("<H", data, coff + 2)[0]
    opt_size = struct.unpack_from("<H", data, coff + 16)[0]
    opt = coff + 20
    file_align = struct.unpack_from("<I", data, opt + 60)[0] or 0x200
    sec_align = struct.unpack_from("<I", data, opt + 56)[0] or 0x1000
    magic = struct.unpack_from("<H", data, opt)[0]

    sec = opt + opt_size
    sections = []
    for i in range(num_sections):
        base = sec + i * 40
        name = data[base:base + 8].split(b"\x00", 1)[0].decode("latin1", "replace")
        vsize, vaddr, rawsize, rawptr = struct.unpack_from("<IIII", data, base + 8)
        chars = struct.unpack_from("<I", data, base + 36)[0]
        sections.append(dict(name=name, vaddr=vaddr, vsize=vsize,
                            rawptr=rawptr, rawsize=rawsize,
                            chars=chars, base=base))

    keep = [s for s in sections if not is_debug_section(s["name"])]
    drop = [s for s in sections if is_debug_section(s["name"])]
    print(f"总节 {num_sections}：保留 {len(keep)}，丢弃调试节 {len(drop)} "
          f"({', '.join(s['name'] for s in drop[:6])}{'...' if len(drop) > 6 else ''})")

    # 头部（DOS+PE+COFF+可选头+新节表）保持原位
    new_sec_table_off = sec
    new_num = len(keep)
    header_end = new_sec_table_off + new_num * 40
    new_sizeofheaders = (header_end + file_align - 1) & ~(file_align - 1)

    # 组装新文件：头部 + 各保留节原始数据（按 FileAlignment 对齐）
    out = bytearray(data[:header_end])  # 先放头部（节表部分稍后回填）
    # 计算新的 PointerToRawData 并写入节数据
    cur_raw = new_sizeofheaders
    new_sections = []
    for s in keep:
        # 对齐当前偏移
        cur_raw = (cur_raw + file_align - 1) & ~(file_align - 1)
        chunk = bytes(data[s["rawptr"]:s["rawptr"] + s["rawsize"]])
        new_sections.append(dict(s, new_rawptr=cur_raw, new_rawsize=s["rawsize"]))
        # 写入节数据（不足 rawsize 不补，后面整体对齐）
        if len(out) < cur_raw:
            out.extend(b"\x00" * (cur_raw - len(out)))
        out.extend(chunk)
        cur_raw += s["rawsize"]
    # 文件末尾按 FileAlignment 对齐
    if len(out) & (file_align - 1):
        out.extend(b"\x00" * (file_align - (len(out) & (file_align - 1))))

    # 修正节表项
    for idx, s in enumerate(new_sections):
        b = new_sec_table_off + idx * 40
        struct.pack_into("<I", out, b + 12, s["vaddr"])
        struct.pack_into("<I", out, b + 16, s["new_rawsize"])
        struct.pack_into("<I", out, b + 20, s["new_rawptr"])
    # NumberOfSections
    struct.pack_into("<H", data, coff + 2, new_num)  # 也写回 out 对应位置
    struct.pack_into("<H", out, coff + 2, new_num)
    # SizeOfHeaders
    struct.pack_into("<I", out, opt + 60, new_sizeofheaders)
    # SizeOfImage：最后一个保留节 VA + align(vsize)
    last = max(new_sections, key=lambda s: s["vaddr"] + s["vsize"])
    new_image = (last["vaddr"] + ((last["vsize"] + sec_align - 1) & ~(sec_align - 1)) + sec_align - 1) & ~(sec_align - 1)
    struct.pack_into("<I", out, opt + 56, new_image)
    # 清零调试目录（IMAGE_DIRECTORY_ENTRY_DEBUG = 6）
    dd_off = (opt + 112) if magic == 0x20B else (opt + 96)
    struct.pack_into("<II", out, dd_off + 6 * 8, 0, 0)

    # 重算校验和
    cs = pe_checksum(out)
    struct.pack_into("<I", out, e_lfanew + 4 + 20 + 64, cs)

    if inplace:
        open(path, "wb").write(out)
        print(f"已就地裁剪：{path} -> {len(out)/1048576:.1f} MB")
    else:
        import os
        base, ext = os.path.splitext(path)
        out_path = base + ".stripped" + ext
        open(out_path, "wb").write(out)
        print(f"已写出裁剪版：{out_path} -> {len(out)/1048576:.1f} MB")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
