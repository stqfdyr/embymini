// Windows: embed the app icon. GPUI embeds the application manifest itself (DPI awareness, common controls).
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // Without this Cargo reruns the script on every source change and misses a changed icon
        println!("cargo:rerun-if-changed=../../src-tauri/icons/icon.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../src-tauri/icons/icon.ico");
        res.compile().expect("embedding the icon");
    }
}
