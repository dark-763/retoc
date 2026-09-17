# Ten fixes for repacking UE 4.26 IoStore containers

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

## 5. External arc block sorting, and `imported_packages` ordering

`zen.rs`, `zen_asset_conversion.rs`

The cooker sorts external arc blocks by `FPackageId` (`non_empty_dependencies.sort_by_key`
in `zen.rs`) — conflating that with `imported_packages` order produces a graph
the loader disagrees with, so the two are kept independent.

An earlier version of this fix concluded `imported_packages` itself isn't
sorted at all, based on `FPackageId` order not matching the original. That
conclusion was wrong — checked directly against the original container's
`StoreEntry.imported_packages` across all 7337 packages with more than one
import, with no rebuild involved:

| ordering tried | exact matches |
|---|---|
| `FPackageId` ascending / descending | 18.0% / 10.0% |
| global `load_order` of the imported package, ascending / descending | 7.9% / 21.0% |
| path length | 22.2% |
| basename only (no path) | 36.8% |
| order of first reference while walking exports 0..N (same traversal that builds dependency arcs) | worse than doing nothing at all |
| **imported package's own name, case-insensitive, alphabetical** | **98.6%** |

Case-insensitive name order wins by a wide margin. It's implemented in
`build_zen_dependency_bundles_legacy`: `imported_package_names` — previously
only populated for the >Initial (UE5.0+) import path — is now populated for
legacy (<= Initial) imports too, purely as a sort key (it isn't serialized
for this container header version, see fix 9, so this has no on-disk effect
beyond the reorder). `imported_packages`, `imported_package_names` and
`external_package_dependencies` are then reordered together by one stable
sort. This is safe regardless of what the arc code does with these arrays:
dependency arcs address packages by ID or by (source package, import index),
never by position in these arrays.

The remaining ~1.4% and the composition question (does `imported_packages`
ever disagree in *content*, not just order) are covered in fix 10.

## 6. `export_bundles_size` was never computed

`zen.rs`

The field was left unfilled. It is `header_size` plus the sum of
`cooked_serial_size` across exports.

## 7. Bundle layout is computed, not lifted from the original container

`zen_asset_conversion.rs`, `compute_bundle_layout`

The cooker splits a package's `export_load_order` (the Create/Serialize
command sequence, which retoc already builds correctly) into bundles. The
rule: cut right after a `Serialize` command when the next command in order is
a `Create` of an export with a *smaller* index. That inversion means the
cooker deferred creating something earlier in the file until something later
in the file finished serializing first — a real cross-branch dependency, not
just traversal order.

A wider version of this rule (cutting on `Serialize`-with-smaller-index too,
not just `Create`) catches more of the real boundaries but also fires on
packages that only ever have one bundle in the original — e.g. a class
default object whose `Serialize` is deferred to the end of the package but
isn't recognized as a CDO by `class_index`. That false split only shows up
once you check *all* packages, not just the ones already known to have
several bundles, which is why the first version of this rule passed its
initial test set and still had to be narrowed. The `Create`-only rule
produces zero false splits across the whole 17095-package project it was
verified against, at the cost of some missed real boundaries.

Result: 1134 of 1243 known multi-bundle packages get an exact bundle count
and layout with no input beyond what retoc already computes; 0 single-bundle
packages are incorrectly split. The 109 remaining packages fall into two
shapes that were tried and abandoned as general rules (see `TASK.md`):
single-export native types (`USoundClass`/`USoundSubmix`-like) that still
split `Create`/`Serialize` into two bundles for reasons that look unrelated
to export order, and Blueprint packages where the real boundary is a
container export closing its bundle right after its children finish
serializing, without an index inversion to key off.

`RETOC_BUNDLE_LAYOUT` is kept as an optional override sitting on top of the
computed rule, for exactly those remaining cases:

```
RETOC_BUNDLE_LAYOUT=path\to\bundle_layout.json
```

When the variable isn't set, conversion runs entirely on the computed rule.
When it is set and the JSON has an entry for a package, that entry wins over
the computed layout; packages missing from the JSON still fall back to the
computed rule. `serial_offset` and the node-to-bundle map are recalculated to
match either source.

## 8. Arc deduplication runs after fixup, not at arc-creation time

`zen_asset_conversion.rs`, `dedup_legacy_dependency_arcs`

The dedup key is still the pair `(from_export_bundle_index, to_export_bundle_index)`
within each imported package's block — that part was already right. The bug
was *when* it ran: at arc-creation time, `from_export_bundle_index` is not yet
a real bundle index. It's a disposable placeholder handed out by a global
counter, because the actual source bundle can only be resolved once every
package has been converted and serialized (`fixup_legacy_external_arcs` runs
in a second pass afterwards, since a dependency may point at a package that
hasn't been processed yet). Deduping placeholders is deduping numbers that
are unique by construction — it doesn't collapse anything, and can even
under-collapse: two different imports of the same package that later resolve
to the identical `(from, to)` pair get deduped correctly today, but only
because the dedup now runs after resolution, not before it.

The fix moves deduplication to run once per package, after
`fixup_legacy_external_arcs` has resolved every placeholder to its real
value, operating directly on the already-serialized graph region of
`package_buffer` (patching `graph_data_size` and shrinking the buffer via
splice, rather than re-serializing the package from its struct — which is
gone by that point anyway).

Verified on the full 17095-package project (`to-zen` without
`RETOC_BUNDLE_LAYOUT`, comparing the serialized graph region against the
original byte-for-byte): arc match rate goes from 0/17095 before this fix to
92.75% (15856/17095) after. All remaining mismatches trace back to the same
109 packages from fix 7 whose bundle count is still wrong — 109 mismatch
directly (a wrong bundle count structurally can't produce the right graph),
and the other 1130 mismatch only because they import one of those 109
(confirmed exhaustively: every one of the 1130 has at least one of the 109 in
its own import list). Deduplication runs strictly after bundle layout is
decided, so it can't fix or worsen those 109 — it's an independent, later
stage acting on whatever layout it's handed.

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

## 10. `imported_packages` order and composition

`zen_asset_conversion.rs`, `asset_conversion.rs`

Fix 5 covers the ordering rule itself and how it was found. This is the rest
of that investigation: what fraction of `StoreEntry.imported_packages`
mismatches were ever about *order* rather than *content*, what's left after
the ordering fix, and one narrow, separate bug the investigation surfaced.

Checked against the full 17095-package project (`to-zen` without
`RETOC_BUNDLE_LAYOUT`): before the fix 5 reorder, 3625 packages had an
`imported_packages` list that didn't match the original. Of those, 3618 had
the exact same set of packages, just a different order — only 7 actually
disagreed on *which* packages are listed, always the same shape: the
original has one package we never emit.

After the fix 5 reorder, mismatches drop to 93 (86 order, still the same 7
composition) — 99.46% of the project exact instead of 78.8%. Both the 86 and
the 7 are understood, at different depths:

**The 86 remaining order mismatches** aren't uniform. 55 fit a further
pattern: the original pulls exactly one package to the front, ahead of the
alphabetical rest. In every case looked at, that package plausibly reads as
a Blueprint's parent class or a similar structural dependency (e.g.
`ItemConditions/Asset.uasset`, the most frequent offender at 47 occurrences,
fits as a base class for several quest-interaction Blueprints) — consistent
with, but not confirmed as, "the class/parent import always gets position 0"
(confirming it would mean cross-referencing `class_index`/`super_index` on
the actual exports, not done here). The other 31 preserve dense clusters of
related sub-imports as a block (e.g. every `AF_Pose_*`/`AF_Additive_*`
animation pose import stays adjacent in both original and ours, just as a
block sitting in a different position in the list) — suggestive of Blueprint
component/graph-node order rather than a flat alphabetical rule. Not pursued
further: 31 packages out of 17095 (0.18%).

**The 7 composition mismatches** turned out to be a genuinely separate,
deeper issue, not a corollary of the ordering question. `asset_conversion.rs`
(`resolve_package_import_internal_legacy`) shows that for
`container_header_version <= Initial`, resolving *which* imported package an
import belongs to is done by search, not by index: it iterates every package
in `imported_packages` and looks for a matching
`legacy_global_import_index()` on one of that package's exports. That means
`imported_packages` for this header version isn't required to correspond 1:1
with anything in `import_map` at all — the original cooker can list a
package here purely as a recorded dependency, with no import table entry
that points at it specifically. Confirmed experimentally: adding code on the
`to-zen` side to also register such "bare" package-only imports (import
table entries whose full name is just the package name, with nothing under
it) had zero effect on any of the 7 - the entry these packages are missing
isn't a bare import either, it has no import table entry at all by the time
`to-zen` sees the legacy asset. The loss happens earlier, during
`to-legacy`: there's nothing in the source zen package's `import_map` to
reconstruct it from. A real fix would mean synthesizing a synthetic import
table entry during `to-legacy`, informed directly by the source container's
own `imported_packages` list, in the opposite direction and a different file
from this investigation - not done here, for 7 packages out of 17095 (0.04%,
none of which are in this project's actual translation patch set).

The reorder itself only touches `imported_packages`,
`imported_package_names` and `external_package_dependencies` in memory - it
was checked not to move anything that matters on disk: `export_count`,
`export_bundle_count` and graph-region byte-for-byte match rate (92.75%) are
identical before and after.

---

## Known gaps

**`load_order` is not computed.** Every package gets 0. The values are carried
over from the unpacked original during chunk substitution. I could not work
out the cooker's rule; if anyone knows it, I would like to hear.

**`export_bundle_count`/graph arcs are still wrong for 109 packages** out of
17095, where the computed rule in fix 7 doesn't find the real bundle boundary
(single-export native types, and Blueprint packages whose boundary isn't
keyed on an index inversion — see fix 7). `RETOC_BUNDLE_LAYOUT` covers all of
them as a fallback when the original container's layout is available.

**`imported_packages` is still wrong for 93 packages** out of 17095 (86
order, 7 composition) — see fix 10 for what's understood about both and why
neither was pursued further.

## Verifying a rebuild

Compare the unpacked rebuild against the unpacked original:

* `load_order` — always 0 on our side, carried over from the original only
  during real `pack-raw` chunk substitution, not by direct `to-zen` (known gap);
* `export_count` — expect zero differences;
* `export_bundle_count` and graph arcs — expect zero differences when
  `RETOC_BUNDLE_LAYOUT` covers the package, otherwise expect the 109 known
  exceptions from fix 7 (plus their import-cascade effect on graph arcs);
* `export_bundles_size` will differ on exactly the packages you modified if the
  replacement strings differ in length — that is expected;
* `imported_packages` should match exactly on all but the 93 known exceptions
  (see fix 10).
