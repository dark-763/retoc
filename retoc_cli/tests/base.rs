//! `to-zen --base`: converting a few packages against the original container has to give
//! the same chunks and the same store entries as converting the whole game and picking
//! those packages out. And `unpack-raw --filter`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const PACKAGE_COUNT: usize = 12;

fn retoc(threads: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_retoc")).args(args).env("RAYON_NUM_THREADS", threads).env("RETOC_COMPRESSION", "Zlib").output().expect("failed to start retoc")
}

fn describe(output: &Output) -> String {
    format!("exit code {:?}\nstdout: {}\nstderr: {}", output.status.code(), String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
}

fn ok(output: Output) {
    assert!(output.status.success(), "expected success, got {}", describe(&output));
}

fn fails_with(output: Output, expected_on_stderr: &str) {
    assert_eq!(output.status.code(), Some(1), "expected exit code 1, got {}", describe(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains(expected_on_stderr), "expected {expected_on_stderr:?} on stderr, got {}", describe(&output));
}

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(name);
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap();
    }
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../retoc/tests/UE4.27")
}

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn package_dir(index: usize) -> String {
    format!("Game/Content/Dir{}/Sub{index}", index % 3)
}

/// The cooked UE 4.27 test package under `count` different paths: up to 4.27 the package
/// name comes from the path, so every copy is a package of its own.
fn write_packages(dir: &Path, indices: impl Iterator<Item = usize>) {
    for index in indices {
        let target = dir.join(package_dir(index));
        fs::create_dir_all(&target).unwrap();
        for extension in ["uasset", "uexp"] {
            fs::copy(fixtures().join(format!("TestModUI.{extension}")), target.join(format!("TestModUI.{extension}"))).unwrap();
        }
    }
}

/// The global container, packed from the three chunks kept as fixtures.
fn write_global(dir: &Path, scratch: &Path) {
    let dump = scratch.join("global-dump");
    fs::create_dir_all(dump.join("chunks")).unwrap();
    for (fixture, chunk_id) in [
        ("LoaderInitialLoadMeta_1.bin", "000000000000000000000007"),
        ("LoaderGlobalNames_1.bin", "000000000000000000000008"),
        ("LoaderGlobalNameHashes_1.bin", "000000000000000000000009"),
    ] {
        fs::copy(fixtures().join(fixture), dump.join("chunks").join(chunk_id)).unwrap();
    }
    fs::write(
        dump.join("manifest.json"),
        r#"{"chunk_paths": {}, "version": "PartitionSize", "mount_point": "../../../", "container_header_version": null, "package_store_entries": {}}"#,
    )
    .unwrap();
    ok(retoc("4", &["pack-raw", s(&dump), s(&dir.join("global.utoc"))]));
}

fn manifest(dump: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(dump.join("manifest.json")).unwrap()).unwrap()
}

/// Every chunk and every store entry of `subset` is in `full`, with the same bytes and
/// the same values. Returns the number of chunks compared.
fn assert_subset_equals_full(subset: &Path, full: &Path) -> usize {
    let (subset_manifest, full_manifest) = (manifest(subset), manifest(full));
    let mut compared = 0;
    for entry in fs::read_dir(subset.join("chunks")).unwrap() {
        let name = entry.unwrap().file_name();
        let in_full = full.join("chunks").join(&name);
        assert!(in_full.exists(), "chunk {name:?} of the subset is not in the full conversion");
        assert!(fs::read(subset.join("chunks").join(&name)).unwrap() == fs::read(&in_full).unwrap(), "chunk {name:?} differs from the full conversion");
        let key = name.to_str().unwrap();
        for section in ["chunk_paths", "package_store_entries"] {
            assert_eq!(subset_manifest[section][key], full_manifest[section][key], "{section} of {key} differs from the full conversion");
        }
        compared += 1;
    }
    assert_eq!(subset_manifest["package_store_entries"].as_object().unwrap().len(), compared);
    compared
}

struct Setup {
    root: PathBuf,
    /// original container and global.utoc
    base: PathBuf,
    /// unpack-raw of the original container
    full_dump: PathBuf,
}

fn setup(name: &str) -> Setup {
    let root = fresh_dir(name);
    let all = root.join("all");
    write_packages(&all, 0..PACKAGE_COUNT);
    let base = root.join("base");
    fs::create_dir_all(&base).unwrap();
    write_global(&base, &root);
    ok(retoc("4", &["to-zen", "--version", "UE4_27", s(&all), s(&base.join("Game.utoc"))]));
    let full_dump = root.join("full-dump");
    ok(retoc("4", &["unpack-raw", s(&base.join("Game.utoc")), s(&full_dump)]));
    Setup { root, base, full_dump }
}

#[test]
fn a_subset_converted_against_the_base_equals_the_full_conversion() {
    let setup = setup("retoc-test-base-subset");

    for (run, indices) in [("three", vec![1usize, 4, 9]), ("one", vec![7usize])] {
        let input = setup.root.join(format!("input-{run}"));
        write_packages(&input, indices.iter().copied());

        let mut outputs = Vec::new();
        for threads in ["1", "4"] {
            let utoc = setup.root.join(format!("out-{run}-{threads}")).join("Patch_P.utoc");
            fs::create_dir_all(utoc.parent().unwrap()).unwrap();
            ok(retoc(threads, &["to-zen", "--version", "UE4_27", "--base", s(&setup.base), s(&input), s(&utoc)]));
            outputs.push(utoc);
        }
        // one thread and four give the same files
        assert!(fs::read(&outputs[0]).unwrap() == fs::read(&outputs[1]).unwrap(), "{run}: .utoc differs between 1 and 4 threads");
        assert!(fs::read(outputs[0].with_extension("ucas")).unwrap() == fs::read(outputs[1].with_extension("ucas")).unwrap(), "{run}: .ucas differs between 1 and 4 threads");

        let dump = setup.root.join(format!("dump-{run}"));
        ok(retoc("4", &["unpack-raw", s(&outputs[0]), s(&dump)]));
        assert_eq!(assert_subset_equals_full(&dump, &setup.full_dump), indices.len(), "{run}: the patch does not hold exactly the input packages");
    }

    fs::remove_dir_all(&setup.root).unwrap();
}

#[test]
fn a_base_that_does_not_fit_stops_the_conversion() {
    let setup = setup("retoc-test-base-misfit");
    let input = setup.root.join("input");
    write_packages(&input, [2usize].into_iter());
    let utoc = setup.root.join("out").join("Patch_P.utoc");
    fs::create_dir_all(utoc.parent().unwrap()).unwrap();

    // Control: the base as it is fits
    ok(retoc("4", &["to-zen", "--version", "UE4_27", "--base", s(&setup.base), s(&input), s(&utoc)]));
    fs::remove_file(&utoc).unwrap();
    fs::remove_file(utoc.with_extension("ucas")).unwrap();

    // A base whose store entry for a package the input does not touch says another load
    // order than the pass computes: the same container, repacked with one number changed.
    let mut edited = manifest(&setup.full_dump);
    let untouched = edited["chunk_paths"].as_object().unwrap().iter().find(|(_, path)| path.as_str().unwrap().contains("/Sub5/")).map(|(id, _)| id.clone()).unwrap();
    let load_order = edited["package_store_entries"][&untouched]["load_order"].as_u64().unwrap();
    edited["package_store_entries"][&untouched]["load_order"] = (load_order + 1).into();
    fs::write(setup.full_dump.join("manifest.json"), serde_json::to_string(&edited).unwrap()).unwrap();
    let misfit = setup.root.join("misfit");
    fs::create_dir_all(&misfit).unwrap();
    for file in ["global.utoc", "global.ucas"] {
        fs::copy(setup.base.join(file), misfit.join(file)).unwrap();
    }
    ok(retoc("4", &["pack-raw", s(&setup.full_dump), s(&misfit.join("Game.utoc"))]));

    fails_with(retoc("4", &["to-zen", "--version", "UE4_27", "--base", s(&misfit), s(&input), s(&utoc)]), "the base does not fit: 1 of its 11 packages");
    assert!(!utoc.exists() && !utoc.with_extension("ucas").exists(), "a refused conversion left output files behind");

    // Other ways a base is refused, each before anything is written
    fails_with(retoc("4", &["to-zen", "--version", "UE4_27", "--base", s(&setup.base.join("Game.utoc")), s(&input), s(&utoc)]), "is not a directory");
    fails_with(retoc("4", &["to-zen", "--version", "UE4_26", "--base", s(&setup.base), s(&input), s(&utoc)]), "--version asks for");
    fails_with(retoc("4", &["to-zen", "--version", "UE4_27", "--base", s(&setup.base), s(&input), s(&utoc.with_file_name("Game.utoc"))]), "have the same name");
    let empty = setup.root.join("empty");
    fs::create_dir_all(&empty).unwrap();
    fails_with(retoc("4", &["to-zen", "--version", "UE4_27", "--base", s(&setup.base), s(&empty), s(&utoc)]), "nothing to convert");
    assert!(!utoc.exists());

    fs::remove_dir_all(&setup.root).unwrap();
}

#[test]
fn unpack_raw_takes_only_the_chunks_the_filter_names() {
    let setup = setup("retoc-test-unpack-filter");
    let container = setup.base.join("Game.utoc");

    let dump = setup.root.join("filtered");
    ok(retoc("4", &["unpack-raw", s(&container), s(&dump), "--filter", "/Dir1/", "--filter", "/Sub6/"]));
    // Dir1 holds the packages 1, 4, 7, 10; Sub6 is one more
    assert_eq!(assert_subset_equals_full(&dump, &setup.full_dump), 5);
    let paths = manifest(&dump);
    assert!(paths["chunk_paths"].as_object().unwrap().values().all(|path| path.as_str().unwrap().contains("/Dir1/") || path.as_str().unwrap().contains("/Sub6/")));

    // the filtered dump packs into a container of its own
    let repacked = setup.root.join("repacked").join("Patch_P.utoc");
    fs::create_dir_all(repacked.parent().unwrap()).unwrap();
    ok(retoc("4", &["pack-raw", s(&dump), s(&repacked)]));
    let again = setup.root.join("again");
    ok(retoc("4", &["unpack-raw", s(&repacked), s(&again)]));
    assert_eq!(assert_subset_equals_full(&again, &setup.full_dump), 5);

    let nothing = setup.root.join("nothing");
    fails_with(retoc("4", &["unpack-raw", s(&container), s(&nothing), "--filter", "NoSuchPath"]), "matched no chunk path");
    assert!(!nothing.exists(), "a refused unpack left its output directory behind");

    fs::remove_dir_all(&setup.root).unwrap();
}
