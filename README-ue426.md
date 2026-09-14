# Nine fixes for repacking UE 4.26 IoStore containers

This fork of [retoc](https://github.com/trumank/retoc) makes it possible not
just to read an IoStore container but to rebuild one the engine will accept.

Tested against a UE 4.26.2 title: 17095 packages, 19779 chunks,
`container_header_version` = `Initial`, TOC version `DirectoryIndex`. The
rebuilt container boots and runs.

---

## 1. Package name hash for `Initial` headers

`zen_asset_conversion.rs`, `gen_hash`

The hash was skipped for the `Initial` header version, so cross-package
imports could not be resolved. The visible symptom was a crash in
`SoundNodeWavePlayer` on the first sound played. The hash is now always
computed.

## 2. Dependency check used the wrong list

`asset_conversion.rs`, `super_index`

The check walked `create_before_create` where it should have walked
`serialize_before_serialize`. The two lists differ, and using the wrong one
produces an incorrect dependency graph.

## 3. `pack-raw` did not write the Container Header

`retoc_cli/src/main.rs`

The critical one: without it the game rejects the rebuilt container outright.
`container_header_version` and `package_store_entries` were added to the raw
manifest, `unpack-raw` now preserves them, and `pack-raw` calls
`write_package_chunk`.

## 4. References to `/Engine/UnknownPackage`

`zen_asset_conversion.rs`

This placeholder appears whenever `to-legacy` cannot resolve an import.
References to it now resolve to `Null` instead of being chased further.

## 5. External arc block sorting

`zen.rs`

The cooker sorts external arc blocks by `FPackageId`. It does **not** sort
`imported_packages` in `StoreEntry` — conflating the two produces a graph the
loader disagrees with.

## 6. `export_bundles_size` was never computed

`zen.rs`

The field was left unfilled. It is `header_size` plus the sum of
`cooked_serial_size` across exports.

## 7. Bundle layout is lifted from the original container

`zen_asset_conversion.rs`

I could not reproduce the rule the cooker uses to split exports into bundles.
Instead the layout is dumped from the original container into JSON and applied
during conversion:

```
RETOC_BUNDLE_LAYOUT=path\to\bundle_layout.json
```

`serial_offset` and the node-to-bundle map are recalculated to match.

This is a workaround rather than a fix, and it is the part of the fork least
suitable for upstreaming as-is.

## 8. Arc deduplication key

`zen_asset_conversion.rs`

The deduplication key is the pair `(package, target bundle)`. It previously
included `from_export_bundle_index`, which at that stage is still a temporary
number, so arcs multiplied instead of collapsing.

## 9. Compression in `pack-raw`

`iostore_writer.rs`

`write_chunk` hardcoded `compression_method_index = 0`, so containers came out
at roughly twice the size of the cooked original — 17.3 GB against 9.
`compression.rs` already provided everything needed; only the call and the
header registration were missing.

Method indexing follows the reader in `lib.rs`: 0 means uncompressed, other
values index `compression_methods` offset by one. A single registered method
therefore gets index 1.

A block is stored compressed only when that is actually smaller; blocks that
do not shrink are written as-is with method 0, which is what the cooker does.
The chunk hash is still computed over uncompressed data.

Gated behind an environment variable so default behaviour is unchanged:

```
RETOC_COMPRESSION=Zlib
```

Result on the same container: 9.62 GB, and a `pack-raw` → `unpack-raw` round
trip reproduces the store entries exactly.

---

## Known gaps

**`load_order` is not computed.** Every package gets 0. The values are carried
over from the unpacked original during chunk substitution. I could not work
out the cooker's rule; if anyone knows it, I would like to hear.

**`export_bundle_count` differs for two engine audio assets.** They live
outside the game's own content, so no bundle layout key is built for them. No
observable effect.

## Verifying a rebuild

Compare the unpacked rebuild against the unpacked original:

* `load_order`, `export_count`, `export_bundle_count` — expect zero differences;
* `export_bundles_size` will differ on exactly the packages you modified if the
  replacement strings differ in length — that is expected;
* `imported_packages` may differ in ordering only (see fix 5).
