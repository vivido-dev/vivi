use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "build/windows.rs"]
mod windows;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build/windows.rs");
    println!("cargo:rustc-check-cfg=cfg(ffmpeg_old_channel_layout)");
    println!("cargo:rustc-check-cfg=cfg(ffmpeg_codecpar_has_framerate)");

    for variable in [
        "PKG_CONFIG_PATH",
        "VCPKG_ROOT",
        "VCPKG_DEFAULT_TRIPLET",
        "VCPKG_TARGET_TRIPLET",
    ] {
        println!("cargo:rerun-if-env-changed={variable}");
    }

    let target = std::env::var("TARGET").unwrap_or_default();
    let windows = target.contains("windows");
    let versions = if windows {
        vcpkg_layout().map(|(include, _)| {
            (
                header_major(
                    &include.join("libavutil/version.h"),
                    "LIBAVUTIL_VERSION_MAJOR",
                ),
                header_major(
                    &include.join("libavcodec/version_major.h"),
                    "LIBAVCODEC_VERSION_MAJOR",
                ),
            )
        })
    } else {
        Some((
            pkg_config_major("libavutil"),
            pkg_config_major("libavcodec"),
        ))
    };
    if versions
        .as_ref()
        .and_then(|versions| versions.0)
        .is_some_and(|major| major < 59)
    {
        println!("cargo:rustc-cfg=ffmpeg_old_channel_layout");
    }
    // libavcodec 62 (FFmpeg 8) inserted `framerate` between
    // `sample_aspect_ratio` and `field_order` in AVCodecParameters. Keep the
    // hand-written ABI binding aligned with the headers selected by pkg-config.
    if versions
        .as_ref()
        .and_then(|versions| versions.1)
        .is_some_and(|major| major >= 62)
    {
        println!("cargo:rustc-cfg=ffmpeg_codecpar_has_framerate");
    }

    let macos = target.contains("apple-darwin");
    let audio_output = macos || target.contains("linux") || windows;

    if windows && std::env::var_os("VCPKG_ROOT").is_some() {
        link_vcpkg_ffmpeg(audio_output);
        return;
    }

    if let Some(link_paths) = emit_pkg_config_libs(audio_output) {
        if windows {
            windows::stage_pkg_config_ffmpeg_runtime(&link_paths);
        }
        return;
    }

    if windows {
        link_vcpkg_ffmpeg(audio_output);
        return;
    }

    if macos {
        println!("cargo:rustc-link-search=native=/opt/homebrew/lib");
        println!("cargo:rustc-link-search=native=/usr/local/lib");
    }

    for library in ["avcodec", "avformat", "avutil"] {
        println!("cargo:rustc-link-lib={library}");
    }
    if audio_output {
        println!("cargo:rustc-link-lib=swresample");
    }
}

fn link_vcpkg_ffmpeg(audio_output: bool) {
    let (_, library_directory) = vcpkg_layout()
        .unwrap_or_else(|| panic!("Vivi requires pkg-config or VCPKG_ROOT on Windows"));
    let mut libraries = vec!["avcodec", "avformat", "avutil"];
    if audio_output {
        libraries.push("swresample");
    }
    for library in &libraries {
        let import_library = library_directory.join(format!("{library}.lib"));
        assert!(
            import_library.is_file(),
            "Vivi requires {}; install ffmpeg with vcpkg",
            import_library.display()
        );
    }
    println!(
        "cargo:rustc-link-search=native={}",
        library_directory.display()
    );
    for library in libraries {
        println!("cargo:rustc-link-lib=dylib={library}");
    }
    let runtime_directory = library_directory
        .parent()
        .expect("vcpkg library directory has no installed triplet parent")
        .join("bin");
    windows::stage_ffmpeg_runtime(&runtime_directory);
}

fn vcpkg_layout() -> Option<(PathBuf, PathBuf)> {
    let root = std::env::var_os("VCPKG_ROOT").map(PathBuf::from)?;
    let triplet = std::env::var("VCPKG_TARGET_TRIPLET")
        .or_else(|_| std::env::var("VCPKG_DEFAULT_TRIPLET"))
        .unwrap_or_else(|_| default_windows_triplet());
    let installed = root.join("installed").join(triplet);
    Some((installed.join("include"), installed.join("lib")))
}

fn default_windows_triplet() -> String {
    match std::env::var("CARGO_CFG_TARGET_ARCH")
        .unwrap_or_default()
        .as_str()
    {
        "x86_64" => "x64-windows",
        "aarch64" => "arm64-windows",
        "x86" => "x86-windows",
        architecture => panic!("unsupported Windows target architecture {architecture:?}"),
    }
    .to_owned()
}

fn header_major(path: &Path, name: &str) -> Option<u32> {
    fs::read_to_string(path).ok()?.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next() == Some("#define") && fields.next() == Some(name))
            .then(|| fields.next()?.parse().ok())
            .flatten()
    })
}

fn emit_pkg_config_libs(audio_output: bool) -> Option<Vec<PathBuf>> {
    let mut libraries = vec!["--libs", "libavformat", "libavcodec", "libavutil"];
    if audio_output {
        libraries.push("libswresample");
    }
    let output = Command::new("pkg-config").args(libraries).output();
    let output = match output {
        Ok(output) if output.status.success() => output,
        _ => return None,
    };

    let mut link_paths = Vec::new();
    for argument in String::from_utf8_lossy(&output.stdout).split_whitespace() {
        if let Some(path) = argument.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={path}");
            link_paths.push(PathBuf::from(path));
        } else if let Some(name) = argument.strip_prefix("-l") {
            println!("cargo:rustc-link-lib={name}");
        }
    }

    Some(link_paths)
}

fn pkg_config_major(library: &str) -> Option<u32> {
    let output = Command::new("pkg-config")
        .args(["--modversion", library])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    String::from_utf8_lossy(&output.stdout)
        .split('.')
        .next()?
        .trim()
        .parse()
        .ok()
}
