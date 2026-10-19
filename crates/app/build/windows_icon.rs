use std::{env, fs, path::PathBuf, process::Command};

/// Embed the Explorer icon in the executable, independently of the Slint window icon.
pub fn embed() {
    println!("cargo:rerun-if-changed=../../assets/icon.ico");
    println!("cargo:rerun-if-env-changed=RC");
    println!("cargo:rerun-if-env-changed=WindowsSdkDir");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let icon = fs::canonicalize("../../assets/icon.ico").expect("Missing assets/icon.ico");
    // Forward slashes avoid backslash escape sequences in the resource script.
    let icon = icon.to_string_lossy().replace('\\', "/");
    let script = output.join("app-icon.rc");
    fs::write(&script, format!("1 ICON \"{icon}\"\n")).unwrap();
    let msvc = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    let resource = output.join(if msvc { "app-icon.res" } else { "app-icon.o" });
    let compiler = env::var_os("RC").map(PathBuf::from).unwrap_or_else(|| {
        if !msvc {
            return PathBuf::from("windres");
        }
        let sdk = env::var_os("WindowsSdkDir")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env::var_os("ProgramFiles(x86)").unwrap_or_default())
                    .join("Windows Kits/10")
            });
        let mut versions: Vec<_> = fs::read_dir(sdk.join("bin"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("x64/rc.exe"))
            .filter(|path| path.is_file())
            .collect();
        versions.sort();
        versions.pop().unwrap_or_else(|| PathBuf::from("rc.exe"))
    });
    let mut command = Command::new(&compiler);
    if msvc {
        command
            .arg("/nologo")
            .arg("/fo")
            .arg(&resource)
            .arg(&script);
    } else {
        command.arg("-i").arg(&script).arg("-o").arg(&resource);
    }
    let result = command.output().unwrap_or_else(|error| {
        panic!(
            "Cannot run resource compiler {}: {error}; set RC to its path",
            compiler.display()
        )
    });
    assert!(
        result.status.success(),
        "Icon resource compilation failed ({}):\n{}\n{}",
        compiler.display(),
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    println!("cargo:rustc-link-arg-bin=mygoimage={}", resource.display());
}
