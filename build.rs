//! Embeds Windows resources into tao.exe and taow.exe: the icon Explorer and
//! Task Manager show, version information (Properties > Details), and an
//! application manifest (`assets/tao.manifest`, which declares long-path
//! awareness).
//!
//! Each executable gets its own resource script, generated into OUT_DIR so
//! that the version and description come straight from Cargo.toml. The
//! script is compiled by `rc.exe` from the Windows SDK, found by
//! `embed-resource`, and linked into that one binary only (not into tests).
//! Nothing happens when building for other systems.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/tao.ico");
    println!("cargo:rerun-if-changed=assets/tao.manifest");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        #[cfg(windows)]
        windows::embed(&[
            (
                "tao",
                &std::env::var("CARGO_PKG_DESCRIPTION").unwrap_or_default(),
            ),
            (
                "taow",
                "Tao background watcher: sorts Downloads, with a tray icon",
            ),
        ]);
    }
}

#[cfg(windows)]
mod windows {
    use std::path::{Path, PathBuf};

    pub fn embed(bins: &[(&str, &str)]) {
        let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
        let assets = Path::new(&std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"))
            .join("assets");
        for (bin, description) in bins {
            let rc = out_dir.join(format!("{bin}.rc"));
            std::fs::write(&rc, resource_script(bin, description, &assets))
                .expect("writing resource script");
            embed_resource::compile_for(&rc, [bin], embed_resource::NONE)
                .manifest_required()
                .expect("compiling Windows resources (rc.exe from the Windows SDK is required)");
        }
    }

    fn resource_script(bin: &str, description: &str, assets: &Path) -> String {
        let env = |k| std::env::var(k).expect("cargo sets CARGO_PKG_VERSION_*");
        let (major, minor, patch) = (
            env("CARGO_PKG_VERSION_MAJOR"),
            env("CARGO_PKG_VERSION_MINOR"),
            env("CARGO_PKG_VERSION_PATCH"),
        );
        let version = env("CARGO_PKG_VERSION");
        // rc string literals escape `"` as `""` and `\` as `\\`.
        let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\"\""));
        let icon = quote(&assets.join("tao.ico").to_string_lossy());
        let manifest = quote(&assets.join("tao.manifest").to_string_lossy());
        let description = quote(description);
        let version = quote(&version);
        let exe = quote(&format!("{bin}.exe"));
        let internal = quote(bin);
        // Numeric constants instead of #include <winres.h>, so no SDK
        // include path is needed: 24 = RT_MANIFEST, resource id 1 =
        // CREATEPROCESS_MANIFEST_RESOURCE_ID (the manifest Windows reads at
        // process start), 0x40004 = VOS_NT_WINDOWS32, 1 = VFT_APP,
        // 0x409/1200 = US English, Unicode.
        format!(
            r#"1 ICON {icon}
1 24 {manifest}

1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEFLAGSMASK 0x3F
FILEFLAGS 0
FILEOS 0x40004
FILETYPE 0x1
FILESUBTYPE 0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "n4ver"
      VALUE "FileDescription", {description}
      VALUE "FileVersion", {version}
      VALUE "InternalName", {internal}
      VALUE "LegalCopyright", "Copyright (c) 2026 n4ver. MIT License."
      VALUE "OriginalFilename", {exe}
      VALUE "ProductName", "Tao"
      VALUE "ProductVersion", {version}
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
        )
    }
}
