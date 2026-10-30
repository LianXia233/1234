#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
对 windows-gnu 构建产物做两件确定性的修复（链接后，不依赖脆弱的原地字节手术）：

1. 移除调试节（/NNN、.debug_* 等 COFF 调试节），把 500+MB 的 debuginfo 体积压下来。
2. 用 windres 把 app.rc（图标 + 版本 + comctl32 v6 manifest）编成单一合法的资源流
   （.res 即 .rsrc 节内容），整体重写 PE：删除旧的 .rsrc 节，把这份新资源作为
   **最后一个节**干净追加，并修正节表 / 资源数据目录 / SizeOfImage / 校验和。

之所以把资源节作为最后一个节追加：GNU ld 无法合并两个 .rsrc 节（我们的资源 +
mingw 强制链入的 default-manifest.o），若原地替换会导致 .rsrc 节 corrupt，产物
在 Windows 加载器层被拒（"此应用无法在你的电脑上运行"）。本脚本做完整重写，
所有偏移自己把控，避免节移位破坏其它节。
"""

import os
import struct
import subprocess
import sys
import tempfile


def align_up(v, a):
    return (v + a - 1) // a * a


def find_windres():
    for t in ("windres", "x86_64-w64-mingw32-windres",
              "i686-w64-mingw32-windres"):
        try:
            subprocess.run([t, "--help"], stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL, check=True)
            return t
        except Exception:
            continue
    sys.exit("错误：找不到 windres，无法编译资源")


def compile_resources(rc_path):
    """用 windres 把 .rc 编成 coff 对象，抽取其中合法的 PE 资源目录（.rsrc 节）。

    注意：不能用 `-O res` —— 那产出的是 Microsoft .res 扁平格式（每条资源前有
    RESOURCEHEADER），并非 PE 的 .rsrc 资源目录树，直接放进去加载器解析会乱码。
    必须用 `-O coff`，从对象里抽 .rsrc 节，那才是 ld 链接 windres 对象时放进
    .rsrc 的同一份合法资源目录。
    """
    windres = find_windres()
    rc_dir = os.path.dirname(os.path.abspath(rc_path))
    # .rc 内相对路径（assets/icon.ico 等）是相对于 assets 的父目录解析的，
    # 因此 windres 的工作目录应设为 assets 的父目录。
    windres_cwd = os.path.dirname(rc_dir)
    rc_rel = os.path.join(os.path.basename(rc_dir),
                          os.path.basename(rc_path))
    tf = tempfile.NamedTemporaryFile(suffix=".o", delete=False)
    tf.close()
    try:
        r = subprocess.run(
            [windres, rc_rel, "-O", "coff", "-o", tf.name],
            cwd=windres_cwd, capture_output=True, text=True)
        if r.returncode != 0:
            sys.exit(f"windres 失败：{r.stderr or r.stdout}")
        data = extract_coff_rsrc(tf.name)
    finally:
        try:
            os.unlink(tf.name)
        except OSError:
            pass
    if not data:
        sys.exit("windres 产出的 .rsrc 资源为空")
    return data


def extract_coff_rsrc(obj_path):
    """从 COFF 对象中抽取 .rsrc 节的数据（合法的 PE 资源目录）。"""
    d = open(obj_path, "rb").read()
    if d[:2] != b"\x4c\x01" and d[:2] != b"\x64\x86":
        # 不是 COFF（0x14c x86 / 0x8664 amd64 机器码）；这里只认 COFF 对象
        pass
    (machine, nsec, ts, symptr, nsym,
     optsize, ch) = struct.unpack_from("<HHIIIHH", d, 0)
    p = 20  # COFF 头后紧跟节表
    for i in range(nsec):
        name = d[p:p + 8].rstrip(b"\x00").decode("latin1", "replace")
        vsize, vaddr, rawsize, rawptr = struct.unpack_from("<IIII", d, p + 8)
        p += 40
        if name == ".rsrc":
            size = max(vsize, rawsize)
            return d[rawptr:rawptr + size]
    sys.exit("windres 对象中未找到 .rsrc 节")


def pe_checksum(data, csum_off):
    """计算 PE 校验和，计算时跳过校验和字段本身的 4 字节。"""
    chk = 0
    n = len(data)
    i = 0
    while i < n:
        if i == csum_off:
            i += 4
            continue
        if i + 2 > n:
            w = data[i]
            chk += w
        else:
            w = struct.unpack_from("<H", data, i)[0]
            chk += w
        chk = (chk & 0xFFFF) + (chk >> 16)
        i += 2
    chk = (chk & 0xFFFF) + (chk >> 16)
    chk = (chk + n) & 0xFFFFFFFF
    return chk


DROP_NAMES = {".rsrc", ".drectve", ".stab", ".stabstr"}


def is_debug_section(name):
    if name in DROP_NAMES:
        return True
    if name.startswith("/"):           # COFF 调试节，如 /4 /21 /114
        return True
    if name.lower().startswith(".debug"):
        return True
    return False


def fix_pe(exe_path, rc_path):
    res_data = compile_resources(rc_path)
    d = bytearray(open(exe_path, "rb").read())

    if d[:2] != b"MZ":
        sys.exit("不是合法的 MZ/PE 文件")
    e_lfanew = struct.unpack_from("<I", d, 0x3C)[0]
    if d[e_lfanew:e_lfanew + 4] != b"PE\x00\x00":
        sys.exit("PE 签名缺失")
    coff = e_lfanew + 4
    (machine, nsec, ts, symptr, nsym,
     optsize, ch) = struct.unpack_from("<HHIIIHH", d, coff)
    if machine != 0x8664:
        print(f"警告：Machine=0x{machine:x}（非 AMD64），继续处理", file=sys.stderr)
    opt = coff + 20
    magic = struct.unpack_from("<H", d, opt)[0]
    if magic != 0x20B:
        sys.exit(f"只支持 PE32+（magic=0x20B），实际 0x{magic:x}")
    secalign = struct.unpack_from("<I", d, opt + 32)[0]
    filealign = struct.unpack_from("<I", d, opt + 36)[0]
    dd = opt + 112
    nrv = struct.unpack_from("<I", d, opt + 108)[0]

    sec_table_start = opt + optsize
    sections = []
    for i in range(nsec):
        b = sec_table_start + i * 40
        name = d[b:b + 8].rstrip(b"\x00").decode("latin1", "replace")
        vsize, vaddr, rawsize, rawptr = struct.unpack_from("<IIII", d, b + 8)
        chars = struct.unpack_from("<I", d, b + 36)[0]
        sdata = bytes(d[rawptr:rawptr + rawsize])
        sections.append({
            "name": name, "vsize": vsize, "vaddr": vaddr,
            "rawsize": rawsize, "rawptr": rawptr, "chars": chars,
            "data": sdata,
        })

    kept = [s for s in sections if not is_debug_section(s["name"])]
    print(f"保留 {len(kept)} 个节，移除 {nsec - len(kept)} 个调试/.rsrc 节")

    new_nsec = len(kept) + 1
    new_sec_table_end = sec_table_start + new_nsec * 40
    H = align_up(new_sec_table_end, filealign)

    # 紧凑排布保留节的原始数据，从 H 开始
    cur = H
    for s in kept:
        rp = align_up(cur, filealign)
        s["rawptr_new"] = rp
        cur = rp + s["rawsize"]
    # 追加新 .rsrc 节
    rs_raw = align_up(cur, filealign)
    rs_vsize = len(res_data)
    rs_rawsize = align_up(rs_vsize, filealign)
    rs_va = align_up(max(s["vaddr"] + s["vsize"] for s in kept), secalign)
    sizeofimg = align_up(rs_va + rs_vsize, secalign)

    # ---- 组装新文件 ----
    out = bytearray()
    out += d[0:e_lfanew]                       # DOS 头 + stub
    out += b"PE\x00\x00"
    # COFF 头（NumberOfSections 更新）
    out += struct.pack("<HHIIIHH", machine, new_nsec, ts, symptr, nsym,
                       optsize, ch)
    # 可选头（拷贝后打补丁）
    opt_bytes = bytearray(d[opt:opt + optsize])
    struct.pack_into("<I", opt_bytes, 56, sizeofimg)        # SizeOfImage
    struct.pack_into("<I", opt_bytes, 60, H)                # SizeOfHeaders
    struct.pack_into("<I", opt_bytes, 64, 0)                # CheckSum（暂清零）
    # 资源数据目录（index 2）：RVA + Size
    struct.pack_into("<II", opt_bytes, 112 + 16, rs_va, rs_vsize)
    # 调试数据目录（index 6）：清空
    if nrv > 6:
        struct.pack_into("<II", opt_bytes, 112 + 48, 0, 0)
    out += opt_bytes
    # 节表：先写保留节（更新 rawptr），再写新 .rsrc
    # 每个节头固定 40 字节：Name(8) VSize(4) VAddr(4) RawSize(4) RawPtr(4)
    #                       PtrReloc(4) PtrLnum(4) NRel(2) NLnum(2) Chars(4)
    for s in kept:
        name8 = s["name"].encode("latin1")[:8].ljust(8, b"\x00")
        out += struct.pack("<8sIIIIIIHHI", name8, s["vsize"], s["vaddr"],
                           s["rawsize"], s["rawptr_new"], 0, 0, 0, 0, s["chars"])
    # 新 .rsrc 节
    name8 = b".rsrc\x00\x00\x00"
    rs_chars = 0x40000040  # CNT_INITIALIZED_DATA | MEM_READ
    out += struct.pack("<8sIIIIIIHHI", name8, rs_vsize, rs_va,
                       rs_rawsize, rs_raw, 0, 0, 0, 0, rs_chars)

    # 补零到 H
    if len(out) < H:
        out += b"\x00" * (H - len(out))
    # 写入保留节原始数据
    for s in kept:
        # 确保 out 至少到 rawptr_new
        if len(out) < s["rawptr_new"]:
            out += b"\x00" * (s["rawptr_new"] - len(out))
        out[s["rawptr_new"]:s["rawptr_new"] + s["rawsize"]] = s["data"]
    # 写入新 .rsrc 数据
    if len(out) < rs_raw:
        out += b"\x00" * (rs_raw - len(out))
    out[rs_raw:rs_raw + rs_rawsize] = res_data + b"\x00" * (rs_rawsize - rs_vsize)

    # 校验和
    csum_off = e_lfanew + 4 + 20 + 64
    chk = pe_checksum(out, csum_off)
    struct.pack_into("<I", out, csum_off, chk)

    open(exe_path, "wb").write(out)
    print(f"已重写 PE：{exe_path}")
    print(f"  SizeOfImage={sizeofimg:#x} 资源节 VA={rs_va:#x} "
          f"大小={rs_vsize} 字节  新文件={len(out)} 字节")


if __name__ == "__main__":
    if len(sys.argv) < 3:
        print("用法: fix_pe.py <exe> <app.rc>", file=sys.stderr)
        sys.exit(1)
    fix_pe(sys.argv[1], sys.argv[2])
