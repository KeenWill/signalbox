# Git tool scale suite

Ordinary tests generate a 20 MiB tracked file and a 200 MiB object database. The
ignored scale suite exercises status, worktree and revision diff, stage, commit,
log, branch creation, branch switching, and a fenced push to a local bare
repository. Its generated fixture contains 3.7 million objects, about 46 GiB of
packed data, 1 GB blobs, 1.7 GB tracked content, and 5,000 branches. These
fixture sizes are test cases, not repository limits.

Run inside `devenv shell` from the repository root. Use a new scratch directory;
the generator refuses to replace an existing directory. Pack payloads and
worktree files are sparse, and index generation uses an external sort with a
bounded memory buffer. The tool test modifies its generated worktree.

```bash
python3 tooling/generate-git-scale.py /tmp/signalbox-git-scale
cargo test --no-fail-fast -p signalbox-tools-git --all-features --no-run
SIGNALBOX_GIT_SCALE_ROOT=/tmp/signalbox-git-scale /usr/bin/time -v cargo test --no-fail-fast -p signalbox-tools-git --all-features tests::scale::every_git_tool_handles_the_generated_scale_repository -- --ignored --exact --nocapture
rm -rf /tmp/signalbox-git-scale
```

Compile before measuring so the reported wall-clock time and maximum resident
set size exclude compilation. Record generator and tool measurements separately.
The generator accepts smaller object, pack, blob, tracked-content, and branch
counts for comparisons; `--help` lists the arguments.
