#!/usr/bin/env python3
"""MiSans 子集化工具。

设计目标（为什么不是"只保留 UI 字面量"）：
  本程序是下载器，界面上会显示**用户任意输入**的 URL 与文件名。
  若只保留源码里出现的字符，中文文件名会全部变成 .notdef 方块。
  因此字符集 = 源码字面量 ∪ GB2312 全字表（6763 字） ∪ ASCII/Latin-1
           ∪ CJK 标点 ∪ 假名 ∪ 常用符号区。

字重策略：
  GPUI 侧用到 REGULAR / MEDIUM / SEMIBOLD / BOLD 四档，
  只保留这四档，把 380 MB 的完整家族裁到可嵌入二进制的规模。

输出格式：TTF（cosmic-text / fontdb 不支持 woff2）。
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

# ---------------------------------------------------------------- 字符集构建


def gb2312_chars() -> set[str]:
    """GB2312 一二级汉字 + 全角符号（区 0xA1-0xF7）。"""
    out: set[str] = set()
    for hi in range(0xA1, 0xF8):
        for lo in range(0xA1, 0xFF):
            try:
                out.add(bytes([hi, lo]).decode("gb2312"))
            except UnicodeDecodeError:
                pass
    return out


def gbk_chars() -> set[str]:
    """GBK 全字表（21003 汉字，含繁体常用字）。

    GB2312 只有 6763 简体字，用户下载港台/日本资源时繁体文件名会缺字。
    GBK 覆盖成本约 +1.3 MB/字重，由 --charset gbk 显式开启。
    """
    out: set[str] = set()
    for hi in range(0x81, 0xFF):
        for lo in range(0x40, 0xFF):
            if lo == 0x7F:
                continue
            try:
                out.add(bytes([hi, lo]).decode("gbk"))
            except UnicodeDecodeError:
                pass
    return out


def ranges(*pairs: tuple[int, int]) -> set[str]:
    out: set[str] = set()
    for a, b in pairs:
        out.update(chr(c) for c in range(a, b + 1))
    return out


def source_literals(root: Path) -> set[str]:
    """抽取 Rust 源码里的字符串/字符字面量。

    粗粒度但足够：所有非 ASCII 字符一律视为需要覆盖的字符，
    避免因解析器不完善而漏字。
    """
    out: set[str] = set()
    for p in root.rglob("*.rs"):
        if "target" in p.parts:
            continue
        try:
            text = p.read_text(encoding="utf-8")
        except (UnicodeDecodeError, OSError):
            continue
        # 去注释后取所有非 ASCII 字符 + 可见 ASCII
        text = re.sub(r"//.*?$", "", text, flags=re.M)
        text = re.sub(r"/\*.*?\*/", "", text, flags=re.S)
        out.update(ch for ch in text if ord(ch) > 0x7F)
        out.update(ch for ch in text if 0x20 <= ord(ch) <= 0x7E)
    return out


def build_charset(src_root: Path, extra: Path | None, want_gbk: bool = False) -> set[str]:
    cs: set[str] = set()
    cs |= ranges(
        (0x20, 0x7E),      # ASCII 可打印
        (0xA0, 0xFF),      # Latin-1 Supplement
        (0x100, 0x17F),    # Latin Extended-A（欧洲文件名）
        (0x2000, 0x206F),  # 通用标点（— – … ‘ ’ “ ” • ‰ ‱ ₽ 等）
        (0x20A0, 0x20BF),  # 货币符号
        (0x2100, 0x214F),  # Letterlike（℃ № ™）
        (0x2190, 0x21FF),  # 箭头
        (0x2200, 0x22FF),  # 数学运算符
        (0x2460, 0x24FF),  # 带圈数字
        (0x2500, 0x257F),  # 制表符（表格框线）
        (0x25A0, 0x25FF),  # 几何图形（■ □ ▲ ▶ ●）
        (0x2600, 0x26FF),  # 杂项符号（★ ☆ ⚡）
        (0x2700, 0x27BF),  # Dingbats（✓ ✗ ➜）
        (0x2B00, 0x2BFF),  # 杂项符号与箭头（⭐ ⬆）
        (0x3000, 0x303F),  # CJK 标点（、。「」【】〜）
        (0x3041, 0x309F),  # 平假名
        (0x30A0, 0x30FF),  # 片假名
        (0xFE10, 0xFE1F),  # 竖排标点
        (0xFF01, 0xFF5E),  # 全角 ASCII
        (0xFF5F, 0xFFEF),  # 半角/全角形式
    )
    cs |= gbk_chars() if want_gbk else gb2312_chars()
    cs |= source_literals(src_root)
    if extra and extra.exists():
        cs |= {ch for ch in extra.read_text(encoding="utf-8") if ord(ch) > 0x7F}
    # 去掉控制字符与代理区
    return {c for c in cs if ord(c) >= 0x20 and not (0xD800 <= ord(c) <= 0xDFFF)}


# ---------------------------------------------------------------- 字重选择

# MiSans 家族命名 -> 保留策略
KEEP_WEIGHTS = {
    "regular": "MiSans-Regular",
    "medium": "MiSans-Medium",
    "semibold": "MiSans-Semibold",
    "demibold": "MiSans-Semibold",
    "bold": "MiSans-Bold",
}

# 小米把每个字重发布成**独立家族**（"MiSans" / "MiSans Medium" /
# "MiSans Semibold"），且 usWeightClass 用 330/380/520 这类非标准值。
# fontdb 按 (family, weight) 检索，若家族名不一致，font_weight(SEMIBOLD)
# 就会退化成"最近的同名字体"，实际拿到 Regular。
# 因此子集化时统一家族名为 "MiSans"，并把 weight 规范化到 CSS 标准档位。
CANON_WEIGHT = {
    "MiSans-Regular": 400,
    "MiSans-Medium": 500,
    "MiSans-Semibold": 600,
    "MiSans-Bold": 700,
}


def normalize_family(path: Path, weight: int) -> None:
    """统一 family/subfamily 命名，使多字重可被 fontdb 视作同一家族。

    只改写「身份类」记录（1/2/3/4/6/16/17）。
    nameID 0 (Copyright) / 7 (Trademark) / 13 (License) / 14 (License URL)
    必须原样保留 —— MiSans 许可协议第 2 条第 4 款要求：
    「您应在 MiSans 字体的任何副本中保留版权声明和本协议」。
    """
    from fontTools.ttLib import TTFont

    style = "Bold" if weight >= 700 else "Regular"
    ps = path.stem  # MiSans-Regular / MiSans-Semibold ...

    font = TTFont(str(path))
    name = font["name"]
    identity = {1, 2, 3, 4, 6, 16, 17}
    # 只摘除身份类记录，法律声明类一律保留
    keep = [r for r in name.names if r.nameID not in identity]
    name.names = keep

    suffix = ps.split("-")[-1]
    records = {
        1: "MiSans",              # Family
        2: style,                 # Subfamily
        3: f"MiSans {suffix}",    # Unique ID
        4: f"MiSans {suffix}",    # Full name
        6: ps,                    # PostScript name
        16: "MiSans",             # Typographic Family
        17: suffix,               # Typographic Subfamily
    }
    for nid, value in records.items():
        name.setName(value, nid, 3, 1, 0x409)   # Windows Unicode BMP English
        name.setName(value, nid, 1, 0, 0)       # Mac Roman

    os2 = font["OS/2"]
    os2.usWeightClass = weight
    # fsSelection: bit0 italic / bit5 bold，其余按常规
    os2.fsSelection = (os2.fsSelection & ~0b100001) | (0b100000 if weight >= 700 else 0)
    font["head"].macStyle = 1 if weight >= 700 else 0

    font.save(str(path))
    font.close()


def pick_fonts(font_dir: Path) -> list[Path]:
    """从解压目录里挑出需要的字重（优先非 Global 子家族、非可变字体）。"""
    cands = [p for p in font_dir.rglob("*") if p.suffix.lower() in (".ttf", ".otf")]
    picked: dict[str, Path] = {}
    for p in sorted(cands):
        stem = p.stem.lower()
        # 跳过可变字体与斜体：GPUI 侧不需要，且体积大
        if "variable" in stem or "italic" in stem or "vf" == stem[-2:]:
            continue
        for key, want in KEEP_WEIGHTS.items():
            if want.lower() == stem:
                picked.setdefault(key, p)
    # 按 KEEP_WEIGHTS 顺序去重（demibold 与 semibold 映射同名）
    ordered: list[Path] = []
    seen: set[Path] = set()
    for key in KEEP_WEIGHTS:
        p = picked.get(key)
        if p and p not in seen:
            seen.add(p)
            ordered.append(p)
    return ordered


# ---------------------------------------------------------------- 子集化


def subset(src: Path, dst: Path, unicodes: list[int]) -> tuple[int, int]:
    dst.parent.mkdir(parents=True, exist_ok=True)
    ufile = dst.with_suffix(".unicodes.txt")
    ufile.write_text(",".join(f"U+{u:04X}" for u in unicodes), encoding="utf-8")
    cmd = [
        sys.executable, "-m", "fontTools.subset",
        str(src),
        f"--unicodes-file={ufile}",
        f"--output-file={dst}",
        "--layout-features=*",   # 保留 kern/liga/vert 等，rustybuzz 需要
        "--name-IDs=*",          # 保留家族名，fontdb 靠它匹配
        "--name-languages=*",
        "--name-legacy",
        "--glyph-names",
        "--symbol-cmap",
        "--legacy-cmap",
        "--notdef-glyph",
        "--notdef-outline",
        "--recommended-glyphs",
        "--drop-tables+=DSIG",   # 签名表，子集后必然失效
        "--no-prune-unicode-ranges",
    ]
    r = subprocess.run(cmd, capture_output=True, text=True)
    ufile.unlink(missing_ok=True)
    if r.returncode != 0:
        raise SystemExit(f"subset 失败 {src.name}:\n{r.stdout}\n{r.stderr}")
    return src.stat().st_size, dst.stat().st_size


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--fonts", required=True, help="MiSans 解压目录")
    ap.add_argument("--src", default="crates", help="Rust 源码根")
    ap.add_argument("--out", default="crates/app/assets/fonts")
    ap.add_argument("--extra", default=None, help="额外字符列表文件")
    ap.add_argument("--charset", default="gb2312", choices=["gb2312", "gbk"],
                    help="中文字符集：gb2312=6763 简体（默认）/ gbk=21003 含繁体")
    ap.add_argument("--weights", default="regular,medium,semibold,bold",
                    help="逗号分隔的字重子集")
    args = ap.parse_args()

    charset = build_charset(
        Path(args.src), Path(args.extra) if args.extra else None,
        want_gbk=(args.charset == "gbk"),
    )
    unicodes = sorted({ord(c) for c in charset})
    print(f"字符集：{len(unicodes)} 个码位（{args.charset}）")

    want = {w.strip().lower() for w in args.weights.split(",") if w.strip()}
    fonts = [p for p in pick_fonts(Path(args.fonts))
             if p.stem.lower().split("-")[-1] in want]
    if not fonts:
        raise SystemExit(f"未找到字重：{sorted(want)}")
    print("选用字重：" + ", ".join(p.name for p in fonts))

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    total_in = total_out = 0
    for p in fonts:
        dst = out / p.name
        a, b = subset(p, dst, unicodes)
        w = CANON_WEIGHT.get(p.stem, 400)
        normalize_family(dst, w)
        c = dst.stat().st_size
        total_in += a
        total_out += c
        print(f"  {p.name:<24} {a/1048576:7.2f} MB -> {c/1048576:6.2f} MB  "
              f"({100*c/a:5.1f}%)  weight={w}")
    print(f"合计 {total_in/1048576:.1f} MB -> {total_out/1048576:.2f} MB "
          f"（压缩到 {100*total_out/total_in:.1f}%）")

    # ── 覆盖校验：源码里出现的每个字符都必须能渲染，否则界面上会出现豆腐块 ──
    from fontTools.ttLib import TTFont

    t = TTFont(str(out / fonts[0].name), lazy=True)
    cmap = t.getBestCmap() or {}
    names = {}
    for r in t["name"].names:
        if r.platformID == 3 and r.langID == 0x409:
            names.setdefault(r.nameID, r.toUnicode())
    t.close()

    missing = sorted(c for c in source_literals(Path(args.src))
                     if ord(c) >= 0x20 and ord(c) not in cmap)
    if missing:
        print(f"⚠ 源码中有 {len(missing)} 个字符未被字体覆盖："
              + "".join(f"U+{ord(c):04X}({c}) " for c in missing[:20]))
    else:
        print("✓ 源码字符 100% 覆盖")

    # ── 合规校验：MiSans 协议第 2 条第 4 款要求副本保留版权声明 ──
    copyright_ = names.get(0, "")
    if "Xiaomi" in copyright_ or "xiaomi" in copyright_.lower():
        print(f"✓ 版权声明已保留：{copyright_[:72]}…")
    else:
        raise SystemExit(f"✗ 版权声明丢失（nameID 0 = {copyright_!r}），"
                         "违反 MiSans 许可协议第 2 条第 4 款")


if __name__ == "__main__":
    main()
