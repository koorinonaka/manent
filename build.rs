/// per-monitor DPI v2 と Common Controls v6 の manifest を埋め込む。
/// `rustc-link-arg` なので `cargo test` のハーネスにも入る (liveback の build.rs と同じ)。
/// アプリのアイコン (`assets/app-icon.ico`、資源 ID 1) も埋める。
fn main() {
    let manifest = std::env::current_dir()
        .unwrap()
        .join("windows-app-manifest.xml");
    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg=/MANIFESTINPUT:{}",
        manifest.to_str().unwrap()
    );

    println!("cargo:rerun-if-changed=assets/app-icon.ico");
    let mut res = winresource::WindowsResource::new();
    // RT_MANIFEST は入れない (set_manifest を呼ばない)。上の link-arg の manifest と ID 1 で衝突する。
    res.set_icon("assets/app-icon.ico");
    res.set("ProductName", "Manent");
    res.set("FileDescription", "Manent");
    if let Err(err) = res.compile() {
        // rc.exe が無い環境でもビルドは通す (アイコン無しの exe になるだけ)。
        println!("cargo:warning=icon embedding failed: {err}");
    }
}
