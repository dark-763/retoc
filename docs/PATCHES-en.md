# Eleven fixes for repacking UE 4.26 IoStore containers

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

Case-insensitive name order wins by a wide margin (this table's own
reference turned out to have the same contamination problem described in
fix 10 below — a later check against a verified-clean unpack found this
rule to be 100% exact, not 98.6%; the relative ranking of the rejected
alternatives isn't expected to change, just this rule's own score). It's
implemented in
`build_zen_dependency_bundles_legacy`: `imported_package_names` — previously
only populated for the >Initial (UE5.0+) import path — is now populated for
legacy (<= Initial) imports too, purely as a sort key (it isn't serialized
for this container header version, see fix 10, so this has no on-disk effect
beyond the reorder). `imported_packages`, `imported_package_names` and
`external_package_dependencies` are then reordered together by one stable
sort. This is safe regardless of what the arc code does with these arrays:
dependency arcs address packages by ID or by (source package, import index),
never by position in these arrays.

The composition question (does `imported_packages` ever disagree in
*content*, not just order) is covered in fix 10.

## 6. `export_bundles_size` was never computed

`zen.rs`

The field was left unfilled. It is `header_size` plus the sum of
`cooked_serial_size` across exports.

## 7. Bundle layout is computed, not lifted from the original container

`zen_asset_conversion.rs`, `compute_bundle_layout`

> **`RETOC_BUNDLE_LAYOUT` is required for shipping builds, not optional.**
> Established by a controlled pair of builds (see `TASK.md`), verified on
> one game — 17095 packages, UE 4.26.2. Both builds used the same code, the
> same pipeline and the same container-header fix (patch 12), so the engine
> read our own header in both. Measured against a clean unpack of the
> original, they were identical on `load_order`, `export_count`,
> `export_bundles_size`, `imported_packages` and `shader_map_hashes`, and
> differed on exactly one field: `export_bundle_count`, on the ten affected
> packages that the translation patch actually replaces. The build with the
> JSON launched and played; the build with the computed layout hung at
> startup — no crash, no error, no log, just "not responding".
>
> That also settles *why*. An earlier run of this experiment was confounded:
> before patch 12 the engine read the original container's header while
> loading our package bytes, so a build without the JSON could not avoid a
> header-versus-data disagreement whatever the computed rule produced. The
> obvious explanation — that the engine needs the two layers to agree rather
> than needing the original layout — is now ruled out. The layers *did*
> agree in the failing build. The bundle layout matters in its own right:
> which exports share a bundle with which, not merely whether the counts are
> consistent.
>
> The computed rule below gets 1134 of 1243 known multi-bundle packages
> exactly right, and the remaining 109 are not a cosmetic rounding error —
> a wrong bundle count on even one package the game loads is enough to hang
> the whole container. If you don't have the original container to generate
> a `RETOC_BUNDLE_LAYOUT` JSON from, treat the computed layout as a
> best-effort fallback only, and **launch the actual game before shipping** —
> retoc's own tooling cannot detect this (see the known-gaps note on
> `warn_if_bundle_layout_uncertain` below, which catches only a small
> fraction of the 109).
>
> Note that the JSON is not total coverage either: it is generated from the
> original container and, on this title, misses two Engine-content packages
> (`MasterSubmixDefault`, `MasterReverbSubmixDefault`) that are also
> multi-bundle. They happen not to be replaced by the translation patch, so
> they keep their original entries, but a different patch set could expose
> them.

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

`RETOC_BUNDLE_LAYOUT` sits on top of the computed rule as a per-package
override, and — see the warning above — is what makes the difference between
a build that boots and one that silently hangs:

```
RETOC_BUNDLE_LAYOUT=path\to\bundle_layout.json
```

When the variable isn't set, conversion runs entirely on the computed rule
(109 known-wrong packages out of 17095, cause of a confirmed startup hang).
When it is set and the JSON has an entry for a package, that entry wins over
the computed layout; packages missing from the JSON still fall back to the
computed rule. `serial_offset` and the node-to-bundle map are recalculated to
match either source.

**Partial, best-effort automated warning**: when a package's layout comes
from the computed rule (not from the JSON), `warn_if_bundle_layout_uncertain`
cross-checks it against the wider heuristic mentioned above (the one
rejected as the production rule for false-positiving on single-bundle
packages) and prints a warning when they disagree on bundle count — that
disagreement means there's no way to tell, from this package's data alone,
which count is right. Measured on the full project: 7 of the 109
known-wrong packages get flagged this way, out of 12 warnings total (5
false positives). The other 102 produce no warning at all — both known
residual shapes (single-export native types, and Blueprint container
exports whose real boundary has no index inversion) have nothing for either
rule to key off, so this can't see them. **Treat the absence of a warning as
"unverified", not as "correct."**

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
`RETOC_BUNDLE_LAYOUT`), against a verified-clean unpack of the original
(see `TASK.md` — an earlier pass here used a reference that turned out to
have been contaminated by an earlier `patch_smart.py` run against it, and
significantly overstated how much was left unexplained; the numbers below
are the corrected ones): before the fix 5 reorder, order-only mismatches
were common. After it, **order matches exactly on all 17095 packages** —
the case-insensitive by-name rule isn't a 98.6%-with-residual heuristic,
it's exact.

**8 packages** still disagree on *composition* (the original lists one
package we never emit). `asset_conversion.rs`
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
it) had zero effect on any of the 8 - the entry these packages are missing
isn't a bare import either, it has no import table entry at all by the time
`to-zen` sees the legacy asset. The loss happens earlier, during
`to-legacy`: there's nothing in the source zen package's `import_map` to
reconstruct it from. A real fix would mean synthesizing a synthetic import
table entry during `to-legacy`, informed directly by the source container's
own `imported_packages` list, in the opposite direction and a different file
from this investigation - not done here, for 8 packages out of 17095 (0.05%,
none of which are in this project's actual translation patch set).

The reorder itself only touches `imported_packages`,
`imported_package_names` and `external_package_dependencies` in memory - it
was checked not to move anything that matters on disk: `export_count`,
`export_bundle_count` and graph-region byte-for-byte match are identical
before and after.

A bug found while investigating this fix — order not surviving
`pack-raw`/`unpack-raw` for a small number of packages, regardless of what
was written — turned out to be a separate, unrelated defect in this fork's
own `pack-raw`, not in anything this fix touches. See fix 11.

## 11. `pack-raw` duplicated the container header chunk

`retoc_cli/src/main.rs`, `action_pack_raw`

A raw chunks directory produced by `unpack-raw` includes a copy of the
container's own header chunk — it has no path, so nothing in
`chunk_paths` points at it, but it's still an ordinary file in `chunks/`.
`action_pack_raw` copied it straight through via `write_chunk_raw`, the
same as any chunk type it doesn't specifically recognize. `IoStoreWriter::
finalize()` then built and wrote its *own*, correct header — from
`manifest.package_store_entries` — under the identical chunk type, at the
very end. Both ended up in the same output container.

`IoStoreContainer::open()` finds "the" header with
`chunks().find(|info| info.id().get_chunk_type() == ContainerHeader)` —
first match wins, with no check against the id it actually expects. The
stale copy, written by the main loop, always sorted earlier in the TOC
than the fresh one from `finalize()`, so `.find()` picked the stale one
every time.

This isn't an edge case — it's present on essentially every real use of
`pack-raw`, since "`unpack-raw` a container, edit some chunks and the
manifest, `pack-raw` it back" is exactly the workflow the tool exists for.
It surfaced as the "phantom" reordering bug under investigation for fix
10: for most packages, the freshly-computed `imported_packages` happened
to already match what the stale header held, so nothing looked wrong;
for the ~85 (of 17095) where they genuinely differed, the stale value won
every time, no matter what the manifest said — confirmed with an
explicitly empty `imported_packages` list for one package still coming
back as the original container's actual (non-empty) value after a round
trip, and with debug output in both `container_header.rs`'s read and
write paths showing the same package at two different positions
depending on which header got parsed — i.e., two distinct header chunks,
not one being misread.

Fix: skip any input chunk of type `ContainerHeader` in the copy loop —
`finalize()` always builds and writes the real one afterwards, so an old
copy is never needed. Verified on the full 17095-package project (the
same `to-zen` → `unpack-raw` → `patch_smart.py` → `pack-raw` →
`unpack-raw` pipeline used throughout this investigation): zero remaining
`imported_packages` differences of any kind, zero differences in every
other `StoreEntry` field, and the chunk count back to the expected 19779
(was 19780 — the duplicate).

This is entirely inside `retoc_cli`, not `container_header.rs` — no
upstream PR needed for this one.

---

## Known gaps

**`load_order` is not computed.** Every package gets 0. The values are carried
over from the unpacked original during chunk substitution. I could not work
out the cooker's rule; if anyone knows it, I would like to hear.

**`export_bundle_count`/graph arcs are still wrong for 109 packages** out of
17095, where the computed rule in fix 7 doesn't find the real bundle boundary
(single-export native types, and Blueprint packages whose boundary isn't
keyed on an index inversion — see fix 7). This is not cosmetic: confirmed by
a controlled pair of builds, identical except for `export_bundle_count` on
the ten of those 109 that the patch replaces, to hang the game at startup
with no crash and no log (see `TASK.md`, "Журнал опыта: сборка №2").
`RETOC_BUNDLE_LAYOUT` covers all 109 as a fallback when the original
container's layout is available — for a shipping build, use it, don't rely
on the computed rule alone. The error is always in one direction: the
computed rule produces *fewer* bundles than the original, never more, so it
is a lower bound rather than an approximation.

**`imported_packages` is still wrong for 8 packages** out of 17095, all
composition (a package the original lists as an untethered dependency with
no import-table entry pointing at it, which `to-legacy` has nothing to
reconstruct it from — see fix 10). Order is no longer an issue: an earlier
count of 93 (86 order + 7 composition) turned out to be measured against a
contaminated reference — see `TASK.md`, "raw_out_0916 оказался грязным
эталоном" — the name-order rule is exact against a clean original.

## Verifying a rebuild

Compare the unpacked rebuild against the unpacked original:

* `load_order` — always 0 on our side, carried over from the original only
  during real `pack-raw` chunk substitution, not by direct `to-zen` (known gap);
* `export_count` — expect zero differences;
* `export_bundle_count` and graph arcs — **use `RETOC_BUNDLE_LAYOUT`** for
  anything you'll actually ship (see the warning in fix 7 — a wrong count
  hangs the game at startup); without it, expect the 109 known exceptions
  from fix 7 (plus their import-cascade effect on graph arcs), and launch
  the game to confirm before shipping regardless;
* `export_bundles_size` will differ on exactly the packages you modified if the
  replacement strings differ in length — that is expected;
* `imported_packages` should match exactly on all but 8 known exceptions
  (see fix 10) — checked against a verified-clean original, not the
  contaminated reference an earlier pass here used.
