fn main() {
    println!("cargo::rustc-check-cfg=cfg(apppilotkit_jni_smoke)");
    println!("cargo::rerun-if-changed=apppilotkit_transport.exports");
    println!("cargo::rerun-if-changed=apppilotkit_transport.map");
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("Cargo sets CARGO_MANIFEST_DIR");
    if std::env::var_os("CARGO_CFG_APPPILOTKIT_JNI_SMOKE").is_some() {
        return;
    }
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("android") => println!(
            "cargo::rustc-cdylib-link-arg=-Wl,--version-script={manifest}/apppilotkit_transport.map"
        ),
        Ok("macos" | "ios") => println!(
            "cargo::rustc-cdylib-link-arg=-Wl,-exported_symbols_list,{manifest}/apppilotkit_transport.exports"
        ),
        _ => {}
    }
}
