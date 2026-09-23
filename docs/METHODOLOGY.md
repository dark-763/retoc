# Methodology

How changes in this fork are measured and verified. These rules exist because
several early results looked successful while the check behind them had not
actually run.

## Running a measurement

- Run shell pipelines with `set -e -o pipefail`. Without `pipefail`, a command
  piped into `head` or `tail` reports the exit code of the last stage, and a
  failure goes unnoticed.
- Record each step's exit code on its own, not through a pipe.
- Check that intermediate files exist between the steps of a pipeline. A step
  that failed leaves the previous run's output in place, and the next step
  will happily consume it.
- Before looking at a result, confirm the change was actually applied: parse
  the edited file, search for the new signature in the source, or look for a
  new string in the built binary. A script that failed to parse changes
  nothing, yet the run still completes with the old configuration.
- Test the code path the change touches, not a neighbouring one. A speed-up in
  block compression cannot be measured with compression turned off.

## Reporting a measurement

- Name the baseline explicitly: the original container, or our own previous
  output. These answer different questions and give different numbers.
- Name the coverage: the whole container, or a subset. A rule validated on a
  known subset can produce false positives outside it.
- Print the compared values next to the verdict, not just the verdict. A
  mistake in the comparison is only visible in the values.
- A result that matches the expectation, especially a round one, is a reason
  to re-check the harness, not to trust it.
- Add a negative control: corrupt the input on purpose and confirm the check
  now fails. A check that cannot fail proves nothing.

## Drawing conclusions

- State what was checked, on what data, and what the check does not show. Do
  not generalise from one successful build to the format.
- A tool warning, or two quantities that must agree but do not, is a lead to
  follow to a named cause. "Probably harmless" is a hypothesis like any other.
- If an implementation follows the reference behaviour exactly and the numbers
  still disagree, look at the input data and how it is read. Do not tune the
  algorithm to match; a fix made for the numbers hides the real cause.

## Comparing containers

A rebuilt container is not byte-reproducible. Conversion runs in parallel and
chunks are packed in the order tasks finish, so two runs of the same command
produce `.utoc`/`.ucas` files with different hashes and identical sizes.
A file hash is therefore not a criterion for a rebuilt container.

Compare at chunk level instead:

- the set of chunk ids;
- the content of each chunk (after decompression);
- every field of each package store entry;
- the per-chunk hash and flags recorded in the TOC.

Keep the output file name identical between the runs being compared. The
container id is derived from it, and the id of the container header chunk is
derived from the container id.

The exception is a build made by replacing chunks inside an existing raw
unpack: there the chunk order is preserved and file hashes are comparable.

## Verifying a rebuild

1. `retoc verify` on the output must succeed. Confirm it can fail: change one
   byte inside a compressed block of a copy and check that it reports a hash
   mismatch.
2. Against the original: the same set of chunk ids; for chunks outside the set
   of replaced packages, identical content; for store entries, zero
   differences in load order, export count and export bundle count.
   Differences in export bundle size are expected only for replaced packages.
3. Against the previous build, when only one input changed: every changed
   chunk must be explained by that input.
4. Launch the game. None of the above sees everything the loader depends on.
