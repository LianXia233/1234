# MiSans 字体（子集化）

本目录的 4 个 `.ttf` 由 [MiSans](https://hyperos.mi.com/font/download) 官方发布包
（`MiSans_Global_ALL.zip` → `MiSans.zip` → `MiSans/ttf/`）经 `tools/subset_fonts.py`
裁切而成，**不是官方原始文件**，请勿单独分发。

## 为什么放在仓库里

跨平台一致性。系统字体（微软雅黑 / 苹方 / Noto Sans CJK）的行高、字宽、标点
宽度各不相同，同一套布局在三个平台上会错位。内嵌字体让进度条、表格列、
等宽数字在任何机器上像素级一致。

## 子集规格

| 项 | 值 |
|---|---|
| 字符集 | ASCII + Latin-1 + Latin Ext-A + GB2312（6763 汉字）+ CJK 标点 + 假名 + 箭头/制表/几何/全角 |
| 码位数 | 9,456 请求 → 7,908 实际覆盖（MiSans 本身不含的符号会被跳过） |
| 每字重 | 8,133 字形 / 1.72–1.74 MB |
| 合计 | 30.6 MB → 6.94 MB（22.7%） |
| 保留表 | `cmap glyf loca head hhea hmtx maxp name OS/2 post` + `GDEF GPOS GSUB` + `vhea vmtx` |
| 移除表 | `DSIG`（子集后必然失效） |

## 关键的 name 表改写

MiSans 官方把**每个字重发布成独立家族**：

```
MiSans-Regular.ttf   → family "MiSans"          usWeightClass 330
MiSans-Medium.ttf    → family "MiSans Medium"   usWeightClass 380
MiSans-Semibold.ttf  → family "MiSans Semibold" usWeightClass 520
MiSans-Bold.ttf      → family "MiSans"          usWeightClass 630
```

fontdb 按 `(family, weight)` 检索。若沿用官方 name 表，`font_family("MiSans")`
+ `font_weight(SEMIBOLD)` 只会在 "MiSans" 这一个家族里挑最近的，也就是
**Regular**——标题不加粗，且不报错。

子集化脚本因此统一改写为 `family="MiSans"` + weight 400/500/600/700，
nameID 16/17（Typographic Family/Subfamily）同步，让四个字重成为一个家族的四档。
`crates/app/tests/fonts.rs` 有对应的回归测试。

## 许可

字体版权归**小米科技有限责任公司**与**汉仪字体**所有，适用
[MiSans 字体知识产权许可协议](./MiSans-LICENSE.pdf)（同目录 `MiSans-LICENSE.pdf`）。

与 SIL OFL 不同，该协议有以下限制，使用前请确认符合你的场景：

- 第 2 条第 1 款：须在软件中注明使用了 MiSans 字体（本项目在顶栏标注）。
- 第 2 条第 2 款：**不得对字体或其组件进行改编或二次开发**。
  子集化属于对字体文件的修改，处于该条款的灰区；若需严格合规，
  应改用 OFL 授权字体（如 Noto Sans SC / 思源黑体）或向小米申请书面授权。
- 第 2 条第 3 款：不得单独再分发字体文件。随应用二进制一同分发属于
  "使用字体创作的应用 App"，协议明确允许。
- 第 2 条第 4 款：副本须保留版权声明与本协议（nameID 0 已保留，PDF 已随附）。

## 重新生成

```bash
# 1. 下载并解出 ttf
curl -LO https://hyperos.mi.com/font-download/MiSans_Global_ALL.zip
unzip -o MiSans_Global_ALL.zip && unzip -o "MiSans Global _ALL/MiSans.zip"

# 2. 子集化（默认 GB2312 档）
python3 tools/subset_fonts.py --fonts MiSans/ttf --src crates --out crates/app/assets/fonts

# 需要繁体覆盖时改用 GBK 档（体积 6.9 MB → 22.7 MB）
python3 tools/subset_fonts.py --fonts MiSans/ttf --charset gbk
```

脚本会输出覆盖校验结果，并断言版权声明未被破坏。
