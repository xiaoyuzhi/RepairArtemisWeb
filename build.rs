use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

/// 找 Windows SDK 的 rc.exe: 环境变量 RC > PATH > Windows Kits 下的最高版本目录。
/// 找不到就硬失败 —— 静默产出一个没有图标和版本号的 exe, 到现场才发现就晚了。
fn find_rc() -> PathBuf {
    if let Ok(p) = env::var("RC") {
        let pb = PathBuf::from(p);
        assert!(pb.is_file(), "环境变量 RC 指向的文件不存在: {}", pb.display());
        return pb;
    }
    if let Ok(out) = Command::new("rc.exe").arg("/?").output() {
        if out.status.success() || !out.stdout.is_empty() {
            return PathBuf::from("rc.exe");
        }
    }
    let kits = r"C:\Program Files (x86)\Windows Kits\10\bin";
    let mut best: Option<(u32, PathBuf)> = None;
    if let Ok(rd) = fs::read_dir(kits) {
        for e in rd.flatten() {
            let rc = e.path().join("x64").join("rc.exe");
            if !rc.is_file() {
                continue;
            }
            // 目录名是 10.0.26100.0 这种四段版本, 按数值比较取最高
            let name = e.file_name().to_string_lossy().to_string();
            let key: u32 = name
                .split('.')
                .nth(2)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            if best.as_ref().map_or(true, |(k, _)| key > *k) {
                best = Some((key, rc));
            }
        }
    }
    best.map(|(_, p)| p)
        .unwrap_or_else(|| panic!("未找到 rc.exe (Windows SDK 资源编译器)。装 VS Build Tools 的 Windows 10/11 SDK, 或设环境变量 RC 指向 rc.exe 全路径。"))
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let rc_file = manifest.join("assets").join("app.rc");
    let ico = manifest.join("assets").join("app.ico");
    println!("cargo:rerun-if-changed={}", rc_file.display());
    println!("cargo:rerun-if-changed={}", ico.display());
    println!("cargo:rerun-if-changed=build.rs");

    let out = env::var("OUT_DIR").unwrap();
    let res = Path::new(&out).join("app.res");
    // cwd 定在 assets: .rc 里的 ICON "app.ico" 是相对路径
    let status = Command::new(find_rc())
        .current_dir(manifest.join("assets"))
        .args(["/nologo", "/fo"])
        .arg(&res)
        .arg("app.rc")
        .status()
        .expect("启动 rc.exe 失败");
    assert!(status.success(), "rc.exe 编译 {} 失败", rc_file.display());
    // link.exe 接受 .res 作为输入文件, 直接把图标与 VERSIONINFO 链进 exe
    println!("cargo:rustc-link-arg={}", res.display());
}
