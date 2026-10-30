//! 内嵌字体：**MiSans**（小米 HyperOS 系统字体）。
//!
//! 为什么内嵌而不是读系统字体：
//! 同一份 UI 在 Windows / macOS / Linux 上要长得一样。系统字体各平台不同
//! （微软雅黑 / 苹方 / Noto），行高、字宽、标点全不一致，进度条对齐全乱。
//!
//! 字体来源：<https://hyperos.mi.com/font/download>（MiSans Global ALL）
//! 子集化：`tools/subset_fonts.py`，见该脚本头部注释。
//!
//! 子集规模（GB2312 档）：
//!   全家族 30.6 MB → 4 字重共 6.9 MB（22.7%）
//!   每字重 8,133 字形 / 7,908 码位：ASCII、Latin-1/Ext-A、GB2312 6763 汉字、
//!   CJK 标点、假名、箭头、制表符、几何图形、全角形式。
//!
//! ⚠ 注意：小米把每个字重发布成**独立家族**（"MiSans" / "MiSans Medium" /
//! "MiSans Semibold"），且 usWeightClass 是 330/380/520 这类非标准值。
//! fontdb 按 (family, weight) 检索，家族名不一致时 `font_weight(SEMIBOLD)`
//! 会退化到 Regular。子集化脚本已把它们统一成 `family="MiSans"` +
//! weight 400/500/600/700，这里是配套的运行时注册。

use std::borrow::Cow;

use gpui::App;

/// 注册到 fontdb 的家族名。UI 侧一律用它，不要硬编码字符串。
pub const FAMILY: &str = "MiSans";

/// MiSans Regular — 正文字重，对应 `FontWeight::NORMAL`(400)。
static REGULAR: &[u8] = include_bytes!("../../assets/fonts/MiSans-Regular.ttf");
/// MiSans Medium — 次级强调，对应 `FontWeight::MEDIUM`(500)。
static MEDIUM: &[u8] = include_bytes!("../../assets/fonts/MiSans-Medium.ttf");
/// MiSans Semibold — 标题/数值，对应 `FontWeight::SEMIBOLD`(600)。
static SEMIBOLD: &[u8] = include_bytes!("../../assets/fonts/MiSans-Semibold.ttf");
/// MiSans Bold — 强强调，对应 `FontWeight::BOLD`(700)。
static BOLD: &[u8] = include_bytes!("../../assets/fonts/MiSans-Bold.ttf");

/// 把四个字重注册进 GPUI 的字体库（fontdb）。
///
/// 必须在任何 `font_family` 生效前调用 —— 也就是 `Application::run` 的
/// 最开头。注册失败不 panic：宁可退回系统字体也不让程序打不开窗口。
pub fn install(cx: &App) {
    let fonts: Vec<Cow<'static, [u8]>> = vec![
        Cow::Borrowed(REGULAR),
        Cow::Borrowed(MEDIUM),
        Cow::Borrowed(SEMIBOLD),
        Cow::Borrowed(BOLD),
    ];
    match cx.text_system().add_fonts(fonts) {
        Ok(()) => tracing::info!(
            family = FAMILY,
            bytes = REGULAR.len() + MEDIUM.len() + SEMIBOLD.len() + BOLD.len(),
            "MiSans 已注册"
        ),
        // 只降级不中断：缺字体时 GPUI 会回退到系统字体，界面仍可用。
        Err(e) => tracing::warn!("MiSans 注册失败，回退系统字体: {e}"),
    }
}
