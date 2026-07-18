fn main() {
    println!("cargo:rerun-if-changed=packaging/icons/windows/zmux.ico");

    #[cfg(windows)]
    winresource::WindowsResource::new()
        .set_icon("packaging/icons/windows/zmux.ico")
        .compile()
        .expect("embedding the zmux Windows application icon");
}
