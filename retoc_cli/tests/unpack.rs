//! unpack and unpack-raw keep where the files of a container belong: its mount point.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn retoc(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_retoc")).args(args).output().expect("failed to start retoc")
}

fn describe(output: &Output) -> String {
    format!("exit code {:?}\nstdout: {}\nstderr: {}", output.status.code(), String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
}

fn assert_success(output: &Output) {
    assert!(output.status.success(), "expected success, got {}", describe(output));
}

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(name);
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap();
    }
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn path_str(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn manifest_mount_point(dump: &Path) -> String {
    let manifest: serde_json::Value = serde_json::from_str(&fs::read_to_string(dump.join("manifest.json")).unwrap()).unwrap();
    manifest["mount_point"].as_str().unwrap().to_string()
}

/// Packs the chunks of `dump` again with another mount point
fn repack_with_mount_point(dump: &Path, mount_point: &str, utoc: &Path) {
    let manifest_path = dump.join("manifest.json");
    let mut manifest: serde_json::Value = serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
    // chunk paths have to start with the mount point; the ones that do not are moved under it
    let old = manifest["mount_point"].as_str().unwrap().to_string();
    for path in manifest["chunk_paths"].as_object_mut().unwrap().values_mut() {
        let current = path.as_str().unwrap().to_string();
        if !current.starts_with(mount_point) {
            *path = format!("{mount_point}{}", current.strip_prefix(&old).unwrap()).into();
        }
    }
    manifest["mount_point"] = mount_point.into();
    fs::write(&manifest_path, serde_json::to_string(&manifest).unwrap()).unwrap();
    fs::create_dir_all(utoc.parent().unwrap()).unwrap();
    assert_success(&retoc(&["pack-raw", path_str(dump), path_str(utoc)]));
}

#[test]
fn unpack_keeps_the_mount_point() {
    let root = fresh_dir("retoc-test-unpack-mount-point");

    // A container with the usual mount point "../../../", built from the cooked UE 4.27 test package
    let input = root.join("input/Game/Content/Mods/MaterialTest");
    fs::create_dir_all(&input).unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../retoc/tests/UE4.27");
    for extension in ["uasset", "uexp"] {
        fs::copy(fixtures.join(format!("TestModUI.{extension}")), input.join(format!("TestModUI.{extension}"))).unwrap();
    }
    let plain = root.join("plain/Test_P.utoc");
    fs::create_dir_all(plain.parent().unwrap()).unwrap();
    assert_success(&retoc(&["to-zen", "--version", "UE4_27", path_str(&root.join("input")), path_str(&plain)]));

    // unpack-raw writes the mount point of the container into the manifest (issue #5) ...
    let dump = root.join("dump");
    assert_success(&retoc(&["unpack-raw", path_str(&plain), path_str(&dump)]));
    assert_eq!(manifest_mount_point(&dump), "../../../");

    // ... and a container mounted deeper keeps its own: pack-raw takes it from the manifest
    let nested = root.join("paks/Nested_P.utoc");
    repack_with_mount_point(&dump, "../../../Game/Content/Mods/", &nested);
    let nested_dump = root.join("nested-dump");
    assert_success(&retoc(&["unpack-raw", path_str(&nested), path_str(&nested_dump)]));
    assert_eq!(manifest_mount_point(&nested_dump), "../../../Game/Content/Mods/");

    // unpack puts the files under the mount point, not straight into the output (issue #33)
    let plain_out = root.join("plain-out");
    assert_success(&retoc(&["unpack", path_str(&plain), path_str(&plain_out)]));
    assert!(plain_out.join("Game/Content/Mods/MaterialTest/TestModUI.uasset").exists());
    let nested_out = root.join("nested-out");
    assert_success(&retoc(&["unpack", path_str(&nested), path_str(&nested_out)]));
    assert!(nested_out.join("Game/Content/Mods/MaterialTest/TestModUI.uasset").exists(), "the directories above the mount point are missing");
    assert_eq!(fs::read(nested_out.join("Game/Content/Mods/MaterialTest/TestModUI.uasset")).unwrap(), fs::read(plain_out.join("Game/Content/Mods/MaterialTest/TestModUI.uasset")).unwrap());

    // unpack takes a directory of containers too (issue #12)
    let dir_out = root.join("dir-out");
    assert_success(&retoc(&["unpack", path_str(&root.join("paks")), path_str(&dir_out)]));
    assert!(dir_out.join("Game/Content/Mods/MaterialTest/TestModUI.uasset").exists());

    // A mount point that climbs out of the output in the middle is refused, nothing is written outside
    let evil = root.join("evil/Evil_P.utoc");
    repack_with_mount_point(&dump, "../../../Game/../../Escaped/", &evil);
    let refused = retoc(&["unpack", path_str(&evil), path_str(&root.join("evil-out"))]);
    assert!(!refused.status.success(), "expected a refusal, got {}", describe(&refused));
    assert!(!root.join("Escaped").exists());

    fs::remove_dir_all(&root).unwrap();
}
