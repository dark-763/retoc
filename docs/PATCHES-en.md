# Repacking UE 4.26 IoStore containers: what this fork changes

This fork of [retoc](https://github.com/trumank/retoc) makes it possible not
just to read an IoStore container but to rebuild one the engine will accept.

Everything here was developed and measured against one UE 4.26.2 title:
17095 packages, 19779 chunks, `container_header_version` = `Initial`, TOC
version `DirectoryIndex`. Translation builds made with this branch were
accepted in game on 20 and 23 September 2026. Nothing here has been checked
on another title or another engine version, and the numbers below are for
this one container.

All engine behaviour described here was worked out from the data and
written up in our own words. No engine source is copied into this
repository. The full record of every measurement, including the ones that
turned out wrong, is in `TASK.md` (Russian).

---

## Part 1. Making a rebuilt container load at all

### 1. Public export hash

`zen_asset_conversion.rs`, `build_zen_export_map`

The hash was first skipped for `Initial` headers, which left cross-package
imports unresolvable and crashed the first sound played. The first fix went
too far and gave every export a hash. The rule the cooker follows is exactly
the `RF_Public` flag. In the original container, 169 597 exports have both
the flag and a hash, 220 467 have neither, and there is no export with one
but not the other. retoc now does the same. `object_flags` come through
`to-legacy` unchanged, so the flag survives the round trip. The number of
export map entries whose hash differs from the original went from 220 467
to 0.

### 2. Dependency check used the wrong list

`asset_conversion.rs`, `super_index`

The check walked `create_before_create` where it should have walked
`serialize_before_serialize`.

### 3. `pack-raw` writes a container header

`retoc_cli/src/main.rs`

Without a header the engine rejects the container outright.
`container_header_version` and `package_store_entries` are carried in the
raw manifest, and `pack-raw` writes the header from them.

### 4. References to `/Engine/UnknownPackage`

`zen_asset_conversion.rs`

`to-legacy` writes this placeholder when an import points at a package that
is not in the container at all. `to-zen` turns references to it into `Null`.
This is the one known source of lasting differences from the original; see
Known gaps.

### 5. `export_bundles_size` is computed

`zen.rs`

It was left unfilled. It is the header size plus the serial sizes of all
exports.

### 6. `imported_packages` order

`zen_asset_conversion.rs`, `build_zen_dependency_bundles_legacy`

The cooker lists a package's imported packages by package name,
alphabetically and case-insensitively. That order is exact on all 17095
packages, measured against a clean unpack of the original. Arc blocks in
the graph are ordered separately, by `FPackageId`, and the two orders are
kept independent.

### 7. `pack-raw` no longer duplicates the container header

`retoc_cli/src/main.rs`, `action_pack_raw`

A raw dump used to contain the source container's own header chunk as an
ordinary file. `pack-raw` copied it through and then wrote its own header,
so the output held two. Readers, the engine included, take the first
header chunk they find, and that was the stale copy. So until this fix the
engine never read the header we built.

`pack-raw` skips such a chunk. Since 21 September `unpack-raw` does not
write it as a file in the first place: the manifest carries everything the
header holds (fix 22).

### 8. Chunk hashes and compression flags match the container version

`lib.rs`, `iostore_writer.rs`

Before UE 5.5 the per-chunk hash in the TOC is SHA-1 of the uncompressed
chunk, padded to 32 bytes. From 5.5 it is a different hash in the same
slot. The reader already told the two apart by version, but the writer and
`retoc verify` always used the 5.5 one. So every rebuilt 4.26 container
carried hashes of the wrong function, and `verify` failed on any real 4.26
container. The hash now follows the container version. The boundary was
drawn by where the field's type changes; only 4.26 was measured.

The container is now marked `Compressed` when it is, and so is each chunk
with at least one compressed block. On the original container that rule
reproduces the flags exactly: 19 778 chunks flagged and 1 not.

---

## Part 2. Computing what used to be copied from the original

Rebuilding a container from legacy assets requires values the legacy format
does not carry: how each package's exports are split into bundles, the
global load order, and the dependency arcs between bundles. For a while the
layout had to be lifted from the original container (a JSON file passed as
`RETOC_BUNDLE_LAYOUT`), and `load_order` was copied over by a script.
Without the JSON, a build hung at startup. This was confirmed by a
controlled pair of builds that differed only in `export_bundle_count` on
ten packages.

All of that is now computed, and the JSON path has been removed.

### 9. A global bundle layout pass

`bundle_layout_pass.rs`, called from `action_to_zen`

The cooker does not split bundles package by package. It builds one graph
over the whole container and walks it, and a package gets a new bundle
whenever the walk comes back to it after visiting something else. No rule
that reads a single package can reproduce that, which is why every
per-package heuristic stalled at about 17 038 of 17 095. The pass works as
follows:

- packages are taken in `FPackageId` order;
- each export becomes two nodes, create and serialize;
- edges come from each package's preload dependency table;
- an edge crosses to another package only through a public export;
- packages are ordered by a depth-first walk over their bare package
  imports, taking the reverse post-order;
- nodes are released in topological order, with a separate ready-queue per
  package.

The pass runs before conversion, over every package in the input, even when
`--filter` narrows the output, so the global numbering is never computed on
a subset. Against the original:

| | result |
|---|---|
| bundle layout | 17 095 of 17 095 packages |
| number of bundles | 18 366, same as the original |
| `load_order` | 0 differences |

On a warm cache the pass costs about 10 seconds.

### 10. The bundle map is filled after the layout is applied

`zen_asset_conversion.rs`

Other packages resolve the source bundle of their arcs through a map from
export to bundle. The map was filled before the package was split, when
every export was still in bundle 0, so every arc into a multi-bundle
package got `from = 0`. On this container that collapsed 1 787 arcs in
1 189 packages.

### 11. The representative of an external arc

`asset_conversion.rs`

The `Initial` format stores an arc only as a pair of bundle numbers, so
`to-legacy` has to pick an object to stand for the source bundle. It used to
take the last entry of that bundle, whatever it was. Often that was a
non-public object, which the cooker would never have let another package
depend on. It now takes the last entry that is public and is already in the
receiving package's import map, preferring the command type of the
bundle's last entry. Two fallbacks exist, each with a warning; neither
fires on this container. Import map differences went from 1 943 packages
to 8.

### 12. Arc order within a block

`zen_asset_conversion.rs`

Three separate things decide the order in which arcs are stored:

- the cooker adds arcs while walking nodes in load order, which for the
  receiving package means bundle order rather than export index order;
- within a node it reads the preload dependency groups in the order the
  table stores them;
- it then sorts each block with a comparator that is not a consistent
  ordering. For arcs with different `from` values, it compares the left
  arc's `from` with the right arc's `to`.

The sort is reproduced as the algorithm the cooker actually runs on short
ranges, a selection sort. Running a generic sort with the same comparator
would not give the same result. No block here has more than four arcs.
Longer blocks would take a different path in the cooker's sort, so retoc
warns about them with the package name.

The evidence for the odd comparator is in the original itself: 28 of
100 991 blocks lie in an order that no ordinary sort produces. One block is
stored in the state the cooker's own sort would move it out of.

Result: 17 091 of 17 095 packages have a graph region identical to the
original, and 100 987 of 100 991 blocks. The other four are covered under
Known gaps.

### 13. Arc deduplication after the source bundle is known

`zen_asset_conversion.rs`

The true `from` of an arc is known only after every package is converted.
Deduplicating before that compared placeholders. It now runs on the
serialized graph region, after the fixup pass.

### 14. No self-redirects in the container header

`zen_asset_conversion.rs`

For `Initial` containers `source_package_name` was always filled in, so
every package got a redirect to itself: 17 095 entries, about 273 KB. The
original has none. The list was also built in task completion order, which
is why the header differed between two runs of the same command.
A redirect is now added only when the source package id differs from the
package's own. Two runs now give identical chunks, header included. The
container files themselves became identical later, see Part 5.

This comes from upstream (`c0ec603`) and affects anyone converting to a UE4
container.

---

## Part 3. Failures that used to look like success

Each of these returned exit code 0, or wrote output, while losing work.

15. **`to-zen`/`to-legacy` with a filter that matches nothing, or an empty
    input**: this wrote a 211-byte container or extracted nothing, with code
    0. It is now an error that names the cause. `to-zen` also opens its
    output files only after the input has been accepted, so a failed run no
    longer truncates an existing container at the output path.
16. **`to-legacy` where every package fails**: it printed
    `Extracted 0 (17095 failed)` and returned 0. Now it fails. See 25 for
    the case where only some of them fail.
17. **A container header that does not parse**: a line on stderr, then
    behaviour as if the container had no packages. It is now an error.
18. **Directory properties taken from `global`**: compression method and
    header were read from the first container, which is always `global` and
    has neither. They now come from a container that has them. `unpack-raw`
    refuses a directory in which several containers carry headers.
19. **A zero-length chunk**: its block range underflowed, and any read of
    such a container failed. `to-zen` writes such a chunk for an empty
    `.ubulk`, so the failure came and went between runs.
20. **Panics on bad input** (no `.utoc` in a directory, an unknown chunk
    type byte) are now messages.
21. **`pack-raw` checks the dump against the manifest** before writing: a
    missing chunk file used to drop the package silently.
22. **The raw manifest carries the compression method and the header**
    (21 September). Without `RETOC_COMPRESSION`, a round trip used to
    produce an uncompressed container twice the size. Localized packages
    and redirects were dropped by `pack-raw`. Both now travel in the
    manifest; the environment variable still wins when set.
23. **Smaller losses**: `manifest` wrote to a fixed path, and listed only
    one of three bulk data kinds. `to-zen` compared `ScriptObjects.bin`
    case-sensitively. `unpack` claimed to have unpacked everything.
24. **Two binary targets named `retoc`** in the workspace: which one ended
    up in `target/release` was undefined. The stray one is removed.
25. **`to-legacy` where some packages fail** (30 September): the count was
    in an info line on stdout and the exit code was 0. Now
    `N of M packages failed to convert` goes to stderr and the exit code is
    1. The packages that did convert are still written, and a `.pak`
    output still gets its index, so one bad asset still does not cost the
    rest. `--allow-partial` turns the error back into a warning.
26. **`to-zen` with an asset that has no `.uexp`** (30 September): the
    asset was skipped with an info line, and the container was written
    without that package. Now the run stops before anything is written.
    `--allow-partial` converts the rest, as before.

Not changed, on purpose: `unpack` still exits with 0 when chunks without a
path are left out (the container header never has one, so every container
would fail), and a shader library whose asset list could not be completed
still only warns. Neither could be exercised on the data at hand.

---

## Part 4. Speed

`iostore_writer.rs`, `iostore.rs`

Compression ran one 64 KB block at a time on a single core. Blocks are now
compressed in parallel and written in order. Packing went from 1 821 s to
233 s, with the output size identical to the byte. Unpacking now reads
chunks in parallel: about 37 s instead of about 180 s.

The parallel compression first caused a deadlock inside `to-zen`, whose
write loop runs inside the global thread pool. Compression now has a
thread pool of its own. The first measurements had missed the deadlock
because they ran `to-zen` without compression, which never reaches the
changed code.

---

## Part 5. The same input gives the same container

`retoc_cli/src/main.rs`, `container_header.rs` (30 September)

Two runs of the same `to-zen` command used to give two different pairs of
files. The chunks were the same; their order was not. Packages were
converted in parallel and written as each worker finished, and everything
in a container follows the order of writing: chunk data in the `.ucas`,
the chunk tables and the directory index in the `.utoc`.

- **`to-zen`** sorts the input listing by path (plain byte order) and
  hands converted packages to the writer in that order. Conversion is
  still parallel. At most `2 * threads` converted packages wait for their
  turn, so memory stays bounded when a large package holds up the ones
  behind it.
- **`pack-raw`** sorts the chunk files by chunk id. It used to take them
  in directory listing order, which is sorted on NTFS and nowhere promised
  to be.
- **`unpack-raw`** writes `manifest.json` from ordered maps. The container
  header kept in the manifest held a hash set and a hash map, which
  serialize in a different order on every run.

Why input order and not chunk id order for `to-zen`: the order has to be
known before conversion, or bulk data cannot go to disk as it arrives and
must be held in memory for a sort at the end. A UE5 package id is only
known once the package has been read. The path is known from the listing.
`pack-raw` has no paths for some chunks, only ids, so it sorts by id.

Measured on the whole game container (17 923 packages, 20 686 chunks,
Zlib), same output name:

- two runs with 8 threads and one with `RAYON_NUM_THREADS=1` give
  byte-identical `.utoc` and `.ucas`; the previous build gives different
  files on every run;
- after `unpack-raw`, every chunk and every `StoreEntry` equals what the
  previous build produces from the same input: only the order changed;
- `pack-raw` of that dump, twice with 8 threads and once with 1, gives
  byte-identical files;
- speed is unchanged within run-to-run noise: 341 s and 356 s against
  334 s and 352 s for the previous build (8 threads, another job running
  on the machine).

What remains outside this: `to-legacy` writing into a `.pak` still adds
entries in completion order. The container id derives from the output file
name, so two outputs with different names are different containers.

Tests: `retoc_cli/tests/determinism.rs`, `retoc_cli/tests/exit_codes.rs`.

---

## Known gaps

- **Arc order in two blocks** (`B_GM_MainMenu`, `B_PLST_Main`). The
  `Initial` format stores arcs per bundle only, so `to-legacy` hangs each
  synthesized dependency on the first entry of the target bundle. How the
  original cook spread them over exports is lost. The set of arcs matches;
  only the order differs.
- **Two phantom arcs `(-1, 0)`** and **8 packages with one fewer
  `imported_packages` entry**, plus `class_index` on 5 and `template_index`
  on 34 export map entries. All of these have one cause: imports of packages
  that are not in this container at all (fix 4).
- **`cooked_serial_offset` on 42 400 export map entries**. The value is
  carried over from the legacy asset, whose header our `to-legacy` writes 2
  to 252 bytes longer than the original cook did. For 4.26 the engine
  derives real offsets from the bundle headers.
- **An extra name `Object` in 2 139 packages**. `to-legacy` has no class to
  write for script imports that are not class default objects, and the
  global container does not record one.
- **One version measured.** The chunk hash boundary (fix 8) and everything in
  Part 2 were measured on 4.26 only.

## Verifying a rebuild

- Two runs of the same command now give the same files (Part 5), so a
  rebuild from the same input with the same build of retoc can be compared
  by hash. Keep the output file name the same between runs, because the
  container id and the id of the header chunk derive from it. Against a
  container written by an older build, or by another tool, the order of
  chunks differs and only a chunk-level comparison means anything: the set
  of chunk ids, the content of each chunk, and every `StoreEntry` field.
- `retoc verify` now works on 4.26 containers. It checks every chunk
  against its TOC hash. To confirm it can catch damage, change one byte
  inside a compressed block of a copy; it should fail.
- Against the original, for a translation patch built from this branch,
  expect 0 differences in `load_order`, `export_count` and
  `export_bundle_count`. `export_bundles_size` should differ only on the
  replaced packages. `imported_packages` should differ on one of them
  (fix 4). Chunks outside the patch list should be unchanged.
- Launch the game before shipping. None of these checks sees everything
  the loader cares about.
