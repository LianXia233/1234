//! Windows 资源（图标 / 版本 / comctl32 v6 manifest）的嵌入由
//! `tools/build-win64.sh` 在链接期完成：它临时用完整 `assets/app.rc` 编出的资源
//! 对象覆盖 mingw 工具链自带的 `default-manifest.o`，让链接器*原生*产出带正确
//! manifest 的合法 PE，之后立即恢复工具链文件。这样可彻底避开 GNU ld 无法合并
//! 两个 `.rsrc` 节的 “.rsrc merge failure”，以及事后手工重写 PE 会把 PE 结构弄坏
//! 导致 Windows 加载器拒载的问题。
//!
//! 因此本 build.rs 不再做任何资源编译，仅声明对资源文件的重编译依赖，避免普通
//! `cargo build` 也强依赖 windres。

fn main() {
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/app.manifest");
    println!("cargo:rerun-if-changed=assets/icon.ico");
}
