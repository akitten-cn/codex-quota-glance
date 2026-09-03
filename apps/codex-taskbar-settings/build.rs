//! 设置页已迁移到与任务栏、详情卡一致的 WebView2 静态视觉稿；无需构建期 UI
//! 编译步骤。保留 build.rs 仅让 Cargo 在视觉稿修改后重新编译内嵌资源。

fn main() {
    println!("cargo:rerun-if-changed=../../prototypes/settings-layout-reference.html");
    println!("cargo:rerun-if-changed=../../prototypes/fluid-front-reference.html");
    println!("cargo:rerun-if-changed=../../prototypes/taskbar-visual-contract.js");
}
