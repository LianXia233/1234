#!/usr/bin/env python3
"""把 PE 导入表里的 icuuc.dll 改成 icu.dll。

背景
----
gpui 0.2.2 依赖的 windows 0.61.x 在 Globalization 模块里声明
`link!("icuuc.dll" ...)`，而 Windows 自带的 ICU 库名是 **icu.dll**
（Windows 10 1703 起随系统提供，导出 u_strlen 等 ICU C API）。
icuuc.dll 是 ICU4C 在 Linux/自带分发时的传统命名，Windows 系统目录里没有，
于是程序在加载阶段就失败：STATUS_DLL_NOT_FOUND。

gpui 只从中用到 `u_strlen` 一个符号（destination_list.rs，拖放文件列表），
icu.dll 同样导出它，所以就地改写导入名即可，不需要替换依赖或打补丁重编译。
两者长度不同，但改写后剩余字节清零即可，不移动任何数据、不改节表。

用法
----
    python3 tools/patch_icu_import.py <exe>            # 改写
    python3 tools/patch_icu_import.py <exe> --check    # 只检查不写
"""
from __future__ import annotations

import struct
import sys

OLD = b"icuuc.dll"
NEW = b"icu.dll"


class Pe:
    def __init__(self, data: bytearray) -> None:
        self.data = data
        if data[:2] != b"MZ":
            raise SystemExit("不是 PE 文件（缺 MZ 头）")
        e_lfanew = struct.unpack_from("<I", data, 0x3C)[0]
        if data[e_lfanew : e_lfanew + 4] != b"PE\0\0":
            raise SystemExit("不是 PE 文件（缺 PE 签名）")
        coff = e_lfanew + 4
        self.num_sections = struct.unpack_from("<H", data, coff + 2)[0]
        opt_size = struct.unpack_from("<H", data, coff + 16)[0]
        self.opt = coff + 20
        magic = struct.unpack_from("<H", data, self.opt)[0]
        if magic == 0x20B:
            dd = self.opt + 112          # PE32+
        elif magic == 0x10B:
            dd = self.opt + 96           # PE32
        else:
            raise SystemExit(f"未知可选头 magic: {magic:#x}")
        self.import_rva, self.import_size = struct.unpack_from("<II", data, dd + 8)
        self.sections = []
        sec = self.opt + opt_size
        for i in range(self.num_sections):
            base = sec + i * 40
            vsize, vaddr, rawsize, rawptr = struct.unpack_from("<IIII", data, base + 8)
            self.sections.append((vaddr, vsize, rawptr, rawsize))

    def off(self, rva: int) -> int:
        for vaddr, vsize, rawptr, rawsize in self.sections:
            span = max(vsize, rawsize)
            if vaddr <= rva < vaddr + span:
                return rawptr + (rva - vaddr)
        raise SystemExit(f"RVA {rva:#x} 不落在任何节内")


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    path = sys.argv[1]
    check_only = "--check" in sys.argv[2:]
    data = bytearray(open(path, "rb").read())
    pe = Pe(data)
    if not pe.import_rva:
        raise SystemExit("没有导入表")

    hits: list[int] = []
    desc = pe.off(pe.import_rva)
    while True:
        name_rva = struct.unpack_from("<I", data, desc + 12)[0]
        if name_rva == 0:
            break
        pos = pe.off(name_rva)
        end = data.index(b"\0", pos)
        dll = bytes(data[pos:end])
        if dll.lower() == OLD:
            hits.append(pos)
        desc += 20

    if not hits:
        print("未发现 icuuc.dll 导入（可能已修补，或依赖版本已改用 icu.dll）")
        return 0

    for pos in hits:
        if check_only:
            print(f"需要修补：偏移 {pos:#x}")
            continue
        # 等长上限内覆盖，剩余字节清零：icuuc.dll\0 -> icu.dll\0\0\0
        room = len(OLD) + 1
        data[pos : pos + room] = NEW.ljust(room, b"\0")
        print(f"已改写导入名：偏移 {pos:#x} icuuc.dll -> icu.dll")

    if not check_only:
        open(path, "wb").write(data)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
