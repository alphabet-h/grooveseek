fn main() {
    // build.rs runs on the host, so cfg!(target_os) reflects the host OS — not
    // the target. Use CARGO_CFG_TARGET_OS to gate on the build target instead.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/app.ico");
        if let Err(e) = res.compile() {
            // app.ico may not yet be a valid multi-size ICO (= PR-1 ship 条件
            // は 32x32 単一サイズ)。warning に留めて continue、explorer での
            // exe icon 表示は best-effort。Phase 5.5 dogfood で blur 観察後に
            // 16/32/48/64/256 multi-size に upgrade を判断。
            println!("cargo:warning=winresource compile failed: {e}");
        }
    }
}
