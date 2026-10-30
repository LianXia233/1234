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
                println!("cargo:rustc-link-arg-bins={}", obj.display());
                return;
            }
            Ok(st) => last_err = format!("{tool} 退出码 {st}"),
            Err(e) => last_err = format!("{tool}: {e}"),
        }
    }
    // 资源（图标/版本号）缺失不影响功能，但要让构建者看见原因。
    println!("cargo:warning=Windows 资源编译失败，将生成无图标版本：{last_err}");
}
