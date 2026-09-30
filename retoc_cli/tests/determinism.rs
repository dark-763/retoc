//! The same input has to give the same container, byte for byte, whatever the number
//! of threads and whichever of them finishes first.
//!
//! These tests run the built binary, because the order of writing is decided in the
//! command line front end, not in the library.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const PACKAGE_COUNT: usize = 48;

fn retoc(threads: &str, args: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_retoc"))
        .args(args)
        .env("RAYON_NUM_THREADS", threads)
        .env("RETOC_COMPRESSION", "Zlib")
        .output()
        .expect("failed to start retoc");
    assert!(
        output.status.success(),
        "retoc {args:?} exited with {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(name);
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap();
    }
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// One cooked UE 4.27 package copied under many paths. Up to UE 4.27 the package name
/// comes from the path, so every copy is a package of its own.
fn write_input(dir: &Path) {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../retoc/tests/UE4.27");
    for index in 0..PACKAGE_COUNT {
        let package_dir = dir.join(format!("Game/Content/Dir{}/Sub{index}", index % 5));
        fs::create_dir_all(&package_dir).unwrap();
        for extension in ["uasset", "uexp"] {
            fs::copy(fixtures.join(format!("TestModUI.{extension}")), package_dir.join(format!("TestModUI.{extension}"))).unwrap();
        }
    }
}

fn assert_same_file(a: &Path, b: &Path) {
    let (left, right) = (fs::read(a).unwrap(), fs::read(b).unwrap());
    assert!(!left.is_empty(), "{a:?} is empty");
    assert!(left == right, "{a:?} ({} bytes) and {b:?} ({} bytes) differ", left.len(), right.len());
}

fn path_str(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn the_same_input_gives_the_same_container() {
    let root = fresh_dir("retoc-test-determinism");
    let input = root.join("input");
    write_input(&input);

    // to-zen: one thread against several, and several twice over. The file name is
    // the same in every run because the container id is derived from it.
    let mut containers = Vec::new();
    for (run, threads) in [("single", "1"), ("parallel-a", "4"), ("parallel-b", "4")] {
        let utoc = root.join(run).join("Test_P.utoc");
        fs::create_dir_all(utoc.parent().unwrap()).unwrap();
        retoc(threads, &["to-zen", "--version", "UE4_27", path_str(&input), path_str(&utoc)]);
        containers.push(utoc);
    }
    for other in &containers[1..] {
        assert_same_file(&containers[0], other);
        assert_same_file(&containers[0].with_extension("ucas"), &other.with_extension("ucas"));
    }

    // unpack-raw: the manifest is the same file on every run
    let mut dumps = Vec::new();
    for (run, threads) in [("raw-a", "1"), ("raw-b", "4")] {
        let dump = root.join(run);
        retoc(threads, &["unpack-raw", path_str(&containers[0]), path_str(&dump)]);
        dumps.push(dump);
    }
    assert_same_file(&dumps[0].join("manifest.json"), &dumps[1].join("manifest.json"));
    assert_eq!(fs::read_dir(dumps[0].join("chunks")).unwrap().count(), PACKAGE_COUNT);

    // pack-raw: again one thread against several
    let mut repacked = Vec::new();
    for (run, threads, dump) in [("repack-a", "1", &dumps[0]), ("repack-b", "4", &dumps[1])] {
        let utoc = root.join(run).join("Test_P.utoc");
        fs::create_dir_all(utoc.parent().unwrap()).unwrap();
        retoc(threads, &["pack-raw", path_str(dump), path_str(&utoc)]);
        repacked.push(utoc);
    }
    assert_same_file(&repacked[0], &repacked[1]);
    assert_same_file(&repacked[0].with_extension("ucas"), &repacked[1].with_extension("ucas"));

    fs::remove_dir_all(&root).unwrap();
}
