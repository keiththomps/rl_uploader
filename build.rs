use std::{env, fs, path::PathBuf, process::Command};
#[path = "src/icon.rs"]
mod icon;

fn main() {
    println!("cargo:rerun-if-changed=src/icon.rs");
    println!("cargo:rerun-if-changed=assets/app.manifest");
    println!("cargo:rerun-if-env-changed=RC");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let ico = out.join("rl-uploader.ico");
    fs::write(&ico, icon::icon_ico()).unwrap();
    let manifest =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("assets/app.manifest");
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let numeric = format!("{},0", version.replace('.', ","));
    let binary = "rl-uploader";
    let description = "Rocket League replay archive and uploader";
    {
        let resource = format!(
            r#"1 ICON "{}"
1 24 "{}"
1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "CompanyName", "RL Uploader"
      VALUE "FileDescription", "{description}"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "RLUploader"
      VALUE "OriginalFilename", "{binary}.exe"
      VALUE "ProductName", "RL Uploader"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x0409, 1200
  END
END
"#,
            ico.to_string_lossy().replace('\\', "/"),
            manifest.to_string_lossy().replace('\\', "/")
        );
        let source = out.join(format!("{binary}.rc"));
        let res = out.join(format!("{binary}.res"));
        fs::write(&source, resource).unwrap();
        let rc = env::var_os("RC").map(PathBuf::from).unwrap_or_else(|| {
            let root =
                PathBuf::from(env::var_os("ProgramFiles(x86)").expect("Windows SDK required"))
                    .join("Windows Kits/10/bin");
            let mut candidates: Vec<_> = fs::read_dir(root)
                .expect("Install the Windows SDK resource compiler")
                .filter_map(|e| e.ok())
                .map(|e| e.path().join("x64/rc.exe"))
                .filter(|p| p.is_file())
                .collect();
            candidates.sort();
            candidates.pop().expect("Windows SDK rc.exe was not found")
        });
        let status = Command::new(&rc)
            .arg("/nologo")
            .arg(format!("/fo{}", res.display()))
            .arg(source)
            .status()
            .expect("Run the Windows SDK resource compiler");
        assert!(status.success(), "Windows resource compilation failed");
        println!("cargo:rustc-link-arg-bin={binary}={}", res.display());
        println!("cargo:rustc-link-arg-bin={binary}=/MANIFEST:NO");
    }
}
