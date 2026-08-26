use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub fn stage_pkg_config_ffmpeg_runtime(link_paths: &[PathBuf]) {
    let runtime_directory = link_paths
        .iter()
        .filter_map(|path| path.parent().map(|parent| parent.join("bin")))
        .find(|path| path.is_dir())
        .unwrap_or_else(|| {
            panic!("pkg-config found FFmpeg import libraries but not its Windows runtime directory")
        });
    stage_ffmpeg_runtime(&runtime_directory);
}

/// Put the native runtime beside Cargo's executable outputs.
///
/// Windows resolves DLL imports before entering `main`. Without an app-local FFmpeg runtime,
/// `vivi.exe` therefore exits through the system loader before it can print a useful diagnostic.
pub fn stage_ffmpeg_runtime(runtime_directory: &Path) {
    let out_directory = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo did not set OUT_DIR"));
    let profile_directory = out_directory
        .ancestors()
        .nth(3)
        .filter(|path| path.join("build").is_dir())
        .unwrap_or_else(|| {
            panic!(
                "unexpected Cargo OUT_DIR layout: {}",
                out_directory.display()
            )
        });

    let entries = fs::read_dir(runtime_directory).unwrap_or_else(|error| {
        panic!(
            "Vivi media runtime directory {} is unavailable: {error}",
            runtime_directory.display()
        )
    });
    let mut staged_families = [false; 4];
    let mut staged_dav1d = false;
    for entry in entries {
        let entry = entry.unwrap_or_else(|error| {
            panic!("could not inspect {}: {error}", runtime_directory.display())
        });
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        let family = ["avcodec-", "avformat-", "avutil-", "swresample-"]
            .iter()
            .position(|prefix| name.starts_with(prefix) && name.ends_with(".dll"));
        if let Some(index) = family {
            staged_families[index] = true;
        } else if name.eq_ignore_ascii_case("dav1d.dll") {
            staged_dav1d = true;
        } else {
            continue;
        }

        let source = entry.path();
        println!("cargo:rerun-if-changed={}", source.display());
        let destination = profile_directory.join(&file_name);
        if !files_are_equal(&source, &destination) {
            fs::copy(&source, &destination).unwrap_or_else(|error| {
                panic!(
                    "could not stage {} beside Cargo executables at {}: {error}",
                    source.display(),
                    destination.display()
                )
            });
        }
    }

    for (present, family) in staged_families.into_iter().zip([
        "avcodec-*.dll",
        "avformat-*.dll",
        "avutil-*.dll",
        "swresample-*.dll",
    ]) {
        assert!(
            present,
            "Vivi requires {family} in {}",
            runtime_directory.display()
        );
    }
    assert!(
        staged_dav1d,
        "Vivi requires dav1d.dll in {}",
        runtime_directory.display()
    );
}

fn files_are_equal(left: &Path, right: &Path) -> bool {
    let Ok(left_metadata) = fs::metadata(left) else {
        return false;
    };
    let Ok(right_metadata) = fs::metadata(right) else {
        return false;
    };
    if left_metadata.len() != right_metadata.len() {
        return false;
    }

    fs::read(left)
        .ok()
        .zip(fs::read(right).ok())
        .is_some_and(|(left, right)| left == right)
}
