//! Windows: the settings window always asks for administrator rights (UAC) when opened.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-arg-bin=rconn-settings=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bin=rconn-settings=/MANIFESTUAC:level='requireAdministrator' uiAccess='false'"
        );
    }
}
