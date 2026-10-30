//! 只为 Windows 目标嵌入图标与版本资源。
//!
//! 注意不能用 `#[cfg(windows)]`：build script 跑在 **host** 上，交叉编译
//! （Linux → Windows）时该条件恒为假，资源就悄悄丢了。这里读 Cargo 提供的
//! `CARGO_CFG_TARGET_OS` 判断*目标*平台。

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/app.manifest");
    println!("cargo:rerun-if-changed=assets/icon.ico");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let target = env::var("TARGET").unwrap_or_default();
    let out = PathBuf::from(env::var("OUT_DIR").expect("缺少 OUT_DIR"));
    let obj = out.join("app_res.o");

    // 交叉编译用带三元组前缀的 windres，本机 Windows 构建则用 PATH 里的 windres。
    let candidates = [
        format!("{target}-windres"),
        "windres".to_string(),
        "x86_64-w64-mingw32-windres".to_string(),
    ];
    let mut last_err = String::new();
    for tool in &candidates {
        match Command::new(tool)
            .args(["assets/app.rc", "-O", "coff", "-o"])
            .arg(&obj)
            .status()
        {
            Ok(st) if st.success() => {
                // 注意：windows-gnu 下 mingw 的 ld 无法合并两个 .rsrc 节
                // （我们的资源 + mingw 默认空 manifest），会导致 .rsrc 节 corrupt。
                // 因此这里只负责把资源编译成 app_res.o 供后续注入脚本抽取，
                // 不再直接链接进 exe（避免与 default-manifest.o 冲突）。
                // 真正的图标/版本/comctl32 v6 manifest 由 tools/embed_manifest.py
                // 在链接后重建并注入。
                println!("cargo:warning=已生成 app_res.o（{0}），将由 embed_manifest.py 注入", obj.display());
                return;
            }
            Ok(st) => last_err = format!("{tool} 退出码 {st}"),
            Err(e) => last_err = format!("{tool}: {e}"),
        }
    }
    // 资源（图标/版本号/manifest）缺失不能静默放行：没有 manifest 的 exe 在
    // 运行时会因 comctl32 v5 缺少 TaskDialogIndirect 直接起不来。宁可编译失败，
    // 也不要产出一份“看起来能编、跑起来就崩”的坏包。
    println!("cargo:warning=Windows 资源（图标/版本/manifest）编译失败：{last_err}");
    eprintln!(
        "build.rs: 无法嵌入 Windows 资源（含 comctl32 v6 manifest）。缺失 windres 会导致运行时 \
         “无法定位程序输入点 TaskDialogIndirect”。请安装 mingw-w64 的 windres \
         （x86_64-w64-mingw32-windres 或 PATH 中的 windres）后重试。"
    );
    std::process::exit(1);
}
