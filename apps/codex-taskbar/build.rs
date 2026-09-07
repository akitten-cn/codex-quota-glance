fn main() {
    println!("cargo:rerun-if-changed=app-icon.ico");
    #[cfg(windows)]
    {
        winresource::WindowsResource::new()
            .set_icon("app-icon.ico")
            .set("ProductName", "Codex Taskbar")
            .set("FileDescription", "Codex Taskbar")
            .compile()
            .expect("Windows application icon resource must compile");
    }
}
