//! Embeds the icon and version info (from Cargo.toml) into hwviewer.exe: Explorer, shortcuts, the installer
//! and antivirus heuristics all look at these.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let v = |k: &str| std::env::var(k).unwrap_or_else(|_| "0".into());
    let (major, minor, patch) =
        (v("CARGO_PKG_VERSION_MAJOR"), v("CARGO_PKG_VERSION_MINOR"), v("CARGO_PKG_VERSION_PATCH"));
    let version = v("CARGO_PKG_VERSION");
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let icon =
        std::path::Path::new(&manifest).join("assets").join("icon.ico").display().to_string().replace('\\', "\\\\");
    let rc = format!(
        r#"1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "CompanyName", "AndradeMatheus"
      VALUE "FileDescription", "HWiNFO Viewer"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "hwviewer"
      VALUE "OriginalFilename", "hwviewer.exe"
      VALUE "ProductName", "HWiNFO Viewer"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    let out = std::path::Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("app.rc");
    std::fs::write(&out, rc).expect("write app.rc");
    embed_resource::compile(&out, embed_resource::NONE).manifest_optional().expect("compile app.rc");
}
