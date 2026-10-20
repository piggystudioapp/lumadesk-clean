fn main() {
    println!("cargo:rerun-if-changed=assets/lumadesk.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/lumadesk.ico")
            .compile()
            .expect("LumaDesk icon resource must compile");
    }
}
