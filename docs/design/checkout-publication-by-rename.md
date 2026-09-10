# Checkout publication by rename design

This design is not built. It scopes a possible extension to the
[Git authority threat model](../spec/git-authority-threat-model.md).
Implementation requires owner review of the ownership and filesystem limitations
below.

## Goal

Publish a completely prepared checkout directory with one atomic namespace
operation, removing recursive quarantine snapshots where exclusive ownership
makes them unnecessary. Preserve foreign data, safe-checkout refusal, and
ambiguous-result reporting.

**Scoping result:** a directory descriptor plus atomic rename is insufficient to
replace the present interference checks in a shared writable checkout. The
rename checks destination absence or exchanges two names; it does not compare an
expected inode or freeze directory contents. The proposed deletion is
conditional on exclusive control of the prepared tree and publication names.
This document adds no implementation, isolation service, configuration gate, or
filesystem fallback.

## Design

### Required semantics and ownership

A publication unit is one prepared subtree and its live directory entry. The
process creates the prepared tree exclusively, retains descriptors for it and
both parents, and finishes writing before publication. Source and destination
must be on the same rename-capable mount. The source name must remain bound to
the prepared tree, and no other permitted writer may change its descendants.

A directory FD supplies a stable reference even when its pathname changes. It
supplies no write exclusion. A same-UID process can traverse a mode-0700 staging
directory; a previously opened writable file or hard link also permits mutation.
These are consequences of the descriptor and access model, not properties that
another snapshot comparison can eliminate. See
[Linux open(2)](https://man7.org/linux/man-pages/man2/open.2.html).

There are two publication operations:

- For an absent destination, no-replace rename publishes the prepared entry only
  if that name is still absent at the namespace update.
- For an existing destination, exchange swaps the prepared and live entries
  atomically, retaining the displaced entry under the staging name. It exchanges
  whatever occupies the destination at that instant, including a foreign
  replacement. There is no expected-inode argument and no combination of
  exchange with no-replace that adds one.

The syscall takes parent descriptors and **names**, not the source object's FD.
Pinning the source alone therefore does not prevent publication of a substituted
source name. The ownership precondition must protect the staging parent/name as
well as the tree. These limits follow directly from the
[Linux rename interface](https://man7.org/linux/man-pages/man2/rename.2.html).

The candidate transition is prepare → publish → finalize index/HEAD → reclaim.
Preparation streams the target bytes into independently owned files; it must not
hard-link writable checkout files. Publication is one syscall per unit under the
existing cooperating-writer serialization. Failure before publication leaves the
live entry alone. Failure after publication retains the displaced data and uses
the existing ambiguous-result contract if restoration cannot be established.
Reclamation of displaced **user** data is a separate ownership problem: an open
writer survives rename. Do not replace its cleanup with an unconditional
recursive delete. An inverse exchange is safe only while both names remain under
the same exclusive control.

This scopes the smallest candidate: subtree publication, not replacing the
configured workspace root. The existing special path handles a tracked directory
becoming a file; its prepared leaf is a file inside a staging directory.
Renaming the staging container itself would change the Git tree shape. That
transition needs either leaf exchange with the same ownership limits or
preparation of a larger parent subtree. Multiple subtree exchanges do not make
the whole checkout atomic.

Replacing the entire root would additionally require its writable parent,
relocation of embedded `.git` administration, preservation of untracked and
ignored entries, and rebinding every workspace authority. A bind-mounted root
cannot simply be exchanged. Existing cwd and open directory handles continue
using the displaced tree. Root exchange is outside the deletion estimate and is
not an implicit way around these constraints.

### Platform and filesystem contract

| Platform / filesystem                                                      | Available guarantee                                                                                                                                                       | Consequence for this design                                                                                                                                                                                                                                       |
| -------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Linux with a supporting filesystem                                         | `renameat2(..., RENAME_NOREPLACE)` rejects an occupied destination. `RENAME_EXCHANGE` atomically swaps existing entries, including nonempty directories and unlike types. | Use descriptor-relative single-component names. Both operations require filesystem support; a Linux build alone is insufficient. Neither provides inode compare-and-swap or content isolation.                                                                    |
| macOS with supporting volume capabilities                                  | `renamex_np` and descriptor-relative `renameatx_np` expose `RENAME_EXCL` and `RENAME_SWAP`.                                                                               | A macOS backend needs the corresponding exclusive/swap capabilities and support for renaming open directories. Prefer `renameatx_np` to avoid ambient path resolution.                                                                                            |
| Other platforms, or a Linux-only `cfg(target_os = "linux")` implementation | No equivalent guarantee follows from compiling an ordinary `rename` fallback.                                                                                             | Keep the current checkout path on supported existing platforms if availability is preserved. This retains its snapshot implementation and tests globally. Disabling checkout there instead is an owner-visible behavior change, not an automatic cfg consequence. |
| Linux NFS client                                                           | The inspected `nfs_rename` rejects nonzero flags with `EINVAL`.                                                                                                           | The proposed exchange/no-replace path is unavailable on this implementation. Do not emulate it with check-then-rename or two/three ordinary renames.                                                                                                              |

Linux documents filesystem-dependent no-replace support, `EXDEV` across mounts,
and possible `EBUSY` for mount points. Runtime unsupported errors must leave
live data untouched; probing must use disposable entries on the actual
publication mount, never the live checkout. A capability probe is evidence about
that mount, not a substitute for handling the actual syscall result.
[Linux rename(2)](https://man7.org/linux/man-pages/man2/rename.2.html)

Apple specifies `VOL_CAP_INT_RENAME_SWAP`, `VOL_CAP_INT_RENAME_EXCL`, and
`VOL_CAP_INT_RENAME_OPENFAIL`; a volume may reject renaming an open directory or
one with open descendants. Exchange and exclusive flags cannot be combined.
Unsupported flags return `ENOTSUP`. The API therefore exists on macOS, but its
presence does not guarantee this design on every mounted volume.
[Apple XNU rename manual](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/rename.2)

Atomic visibility is not crash durability. A durability promise would need file
flushes and the filesystem's directory persistence protocol around the rename;
Linux `fsync` of a file alone does not persist its parent directory entry. This
proposal does not claim an atomic durable transaction spanning checkout, index,
HEAD, and reflog.
[Linux fsync(2)](https://man7.org/linux/man-pages/man2/fsync.2.html)

### NFS and deployment evidence

NFSv4.0 `RENAME` takes two directory handles and two names; it has no exchange
or no-replace flag. It atomically renames compatible entries and may replace an
existing target; a target directory must be empty. Its returned change metadata
is not an expected-inode precondition. NFS COMPOUND is not a general transaction
that makes a separate check and rename indivisible.
[RFC 7530, sections 15.2 and 16.27](https://www.rfc-editor.org/rfc/rfc7530.html)

The Linux client rejection is present in both the pinned
[Linux v6.12 NFS implementation](https://github.com/torvalds/linux/blob/v6.12/fs/nfs/dir.c)
and the inspected upstream `nfs_rename`. Supporting the server's backing
filesystem does not confer rename flags on the NFS client. Existing code already
uses flagged renames for index/reference publication; preserving that code does
not establish NFS checkout support either.

NFS failure can have an uncertain outcome: the server may perform a rename and
lose the reply before a retried request fails. Do not interpret every error as
proof that publication did not occur or blindly retry an exchange. Cross-client
caching and the server's open-handle behavior also require deployment tests.
[Linux rename(2), NFS caveat](https://man7.org/linux/man-pages/man2/rename.2.html)

The deployment requirement includes NFS-backed storage. The scoping session's
`findmnt -T /home/wkg/worktrees/wt-lane139` reports an **ext4 bind mount** in
its visible mount namespace. That observation is not an NFS test and does not
establish the storage presented to the daemon. No NFS or macOS runtime test has
been performed for this document. Before implementation, identify the actual
client mount, kernel, NFS protocol/server and both publication parents. If they
use the Linux NFS path described above, this candidate cannot supply the
required operations there; a different publication contract needs owner
acceptance.

### Quarantine disposition and line estimate

The inventory is against source commit
`659582ec545164a7bb9e9d15d43541a256322bfd`. The ranges below partition **every
case-insensitive `quarantine` matching line** in the four requested files: 280
lines in total, with no missing or duplicate matches. A row groups references
within one helper or explicitly named plumbing region. Counts are matching
source lines, not identifier occurrences. The old/new estimates count affected
lines, including delimiters; they are not the number of matching lines.

**Delete and Replace are conditional future dispositions**, requiring the
ownership contract above. Keep means zero changed lines, not removal of that
site's behavior. With the current shared staging namespace, all these deletions
remain unapproved. Retaining the old path as a platform fallback also prevents
global removal of the snapshot helpers.

#### executor.rs

[executor.rs](../../crates/tools-git/src/executor.rs): **90 matching lines**.

| Lines and site                                                           | Matches | Action  | Estimated old / new lines | Reason                                                                                                                                                         |
| ------------------------------------------------------------------------ | ------: | ------- | ------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 33–33: `import`                                                          |       1 | Replace | 1 / 1                     | Import the owned publication state; remove the snapshot import.                                                                                                |
| 76–83: `QuarantinedCheckoutDirectory`                                    |       5 | Replace | 8 / 10–20                 | Keep source/target handles and publication state; remove both recursive snapshots.                                                                             |
| 85–102: `BranchSwitchHooks`                                              |       4 | Replace | 18 / 16–20                | Move snapshot hooks to preparation/publication boundaries; retain other hooks.                                                                                 |
| 847–956: `quarantine_checkout_directories`                               |      22 | Replace | 110 / 60–100              | Prepare an owned replacement unit, preserving safe-checkout admission.                                                                                         |
| 958–1030: `publish_quarantined_checkout_targets`                         |       6 | Replace | 73 / 35–60                | One exchange or no-replace per unit, under the ownership preconditions.                                                                                        |
| 1032–1041: `preserve_changed_target_quarantines`                         |       2 | Delete  | 10 / 0                    | Only if foreign mutation of the prepared target is excluded.                                                                                                   |
| 1043–1055: `cleanup_published_quarantines`                               |       4 | Replace | 13 / 15–30                | Clean owned preparation only; retain displaced user data until safe reclamation.                                                                               |
| 1057–1091: `restore_unmodified_quarantined_directories`                  |       6 | Replace | 35 / 25–50                | Restore under serialization; a blind inverse exchange is unsafe after interference.                                                                            |
| 1093–1347: `branch_switch wrappers and branch_switch_with_hooks`         |      26 | Replace | 35–55 / 25–45             | Retarget all quarantine hook plumbing, including branch_switch_with_quarantine_hook and branch_switch_with_quarantine_snapshot_hook; preserve unrelated hooks. |
| 1539–1608: `branch_switch_with_hooks publication/rollback orchestration` |      14 | Replace | 70 / 45–75                | Wire preparation, exchange, retention and rollback; preserve error classification.                                                                             |

#### descriptor.rs

[descriptor.rs](../../crates/tools-git/src/descriptor.rs): **50 matching
lines**.

| Lines and site                                                          | Matches | Action | Estimated old / new lines | Reason                                                                                                                               |
| ----------------------------------------------------------------------- | ------: | ------ | ------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| 23–23: `MAX_QUARANTINE_DEPTH`                                           |       1 | Delete | 1 / 0                     | Remove only with the recursive snapshot walkers and their test dependency.                                                           |
| 78–84: `QuarantineDirectory`                                            |       1 | Keep   | 0 / 0                     | Shared pinned temporary-directory owner remains needed by index/reference cleanup.                                                   |
| 86–161: `QuarantineSnapshot, QuarantineSnapshotEntry and all methods`   |       7 | Delete | 76 / 0                    | Remove capture, without_subtree, entry_identity, subtree and nested_under only when the ownership gate is met.                       |
| 163–246: `QuarantineDirectory creation/access/retention methods`        |       4 | Keep   | 0 / 0                     | Includes exclusive temporary creation, test hook, descriptor/name/keep and empty removal; reuse without treating an FD as exclusion. |
| 248–288: `snapshot and clear_if_unchanged methods, including test hook` |       5 | Delete | 41 / 0                    | Prepared-tree cleanup becomes ownership-based; displaced user trees cannot use blind recursive deletion.                             |
| 290–391: `snapshot_pinned_directory_bounded`                            |       4 | Delete | 102 / 0                   | Eliminate recursive metadata, path map and content digest capture.                                                                   |
| 393–471: `clear_snapshot_entries_bounded`                               |       5 | Delete | 79 / 0                    | Eliminate snapshot-driven recursive deletion only after reclamation is safe.                                                         |
| 473–511: `remove_quarantine_directory_if_identity and Drop`             |       2 | Keep   | 0 / 0                     | Shared empty-directory cleanup; no claim that its name check is an inode CAS.                                                        |
| 766–842: `remove_entry_if_identity_with_hook and test wrapper`          |      21 | Keep   | 0 / 0                     | Generic single-entry removal is separate from publishing a prepared checkout directory.                                              |

#### index_lock.rs

[index_lock.rs](../../crates/tools-git/src/index_lock.rs): **55 matching
lines**.

| Lines and site                                         | Matches | Action | Estimated old / new lines | Reason                                                                      |
| ------------------------------------------------------ | ------: | ------ | ------------------------- | --------------------------------------------------------------------------- |
| 22–22: `import`                                        |       1 | Keep   | 0 / 0                     | Shared quarantine owner.                                                    |
| 712–749: `remove_displaced_index_if_current_with_hook` |      13 | Keep   | 0 / 0                     | Displaced live index can be foreign; directory publication does not own it. |
| 751–760: `remove_displaced_index_with_test_hook`       |       2 | Keep   | 0 / 0                     | Keep foreign-index recovery regression hook.                                |
| 762–862: `rollback_index_exchange_if_current`          |      26 | Keep   | 0 / 0                     | Index exchange and its rollback remain a separate transaction.              |
| 864–897: `restore_quarantined_index_exchange`          |       9 | Keep   | 0 / 0                     | Preserve both entries when restoration is obstructed.                       |
| 899–909: `remove_quarantined_index_if_current`         |       4 | Keep   | 0 / 0                     | Only remove the index publication this lock still owns.                     |

#### reference_lock.rs

[reference_lock.rs](../../crates/tools-git/src/reference_lock.rs): **85 matching
lines**.

| Lines and site                                        | Matches | Action | Estimated old / new lines | Reason                                                               |
| ----------------------------------------------------- | ------: | ------ | ------------------------- | -------------------------------------------------------------------- |
| 22–22: `import`                                       |       1 | Keep   | 0 / 0                     | Shared quarantine owner.                                             |
| 90–105: `ReferencePublicationHooks generic and field` |       2 | Keep   | 0 / 0                     | Retain finalization interleaving hook.                               |
| 298–316: `publish_with_hook`                          |       1 | Keep   | 0 / 0                     | Default finalization hook.                                           |
| 318–489: `publish_with_hooks`                         |       4 | Keep   | 0 / 0                     | Generic/destructuring/call plumbing for live-reference finalization. |
| 492–512: `publish_with_test_hooks`                    |       1 | Keep   | 0 / 0                     | Default finalization hook.                                           |
| 514–533: `publish_with_cleanup_test_hook`             |       1 | Keep   | 0 / 0                     | Default finalization hook.                                           |
| 535–554: `publish_with_finalization_test_hook`        |       3 | Keep   | 0 / 0                     | Displaced-reference mutation injection.                              |
| 556–575: `publish_with_pre_exchange_test_hook`        |       1 | Keep   | 0 / 0                     | Default finalization hook.                                           |
| 577–600: `publish_with_rollback_test_hook`            |       1 | Keep   | 0 / 0                     | Default finalization hook.                                           |
| 710–771: `finalize_reference_exchange_if_current`     |      17 | Keep   | 0 / 0                     | Validate/remove displaced reference, preserve foreign data.          |
| 773–806: `remove_published_reference_if_current`      |       8 | Keep   | 0 / 0                     | Removal of an owned live-reference publication.                      |
| 808–831: `restore_quarantined_publication`            |      10 | Keep   | 0 / 0                     | Restore displaced/publication pair independently.                    |
| 833–859: `restore_or_remove_quarantined_reference`    |       7 | Keep   | 0 / 0                     | No-replace restoration and conditional cleanup.                      |
| 861–962: `rollback_reference_exchange_if_current`     |      28 | Keep   | 0 / 0                     | Reference name is outside the checkout-directory exchange.           |

The conditional estimate removes about **300 snapshot-specific descriptor
lines** and replaces about **370–390 executor lines with 232–401 lines**. The
shared index/reference quarantine operations have **zero proposed deletions**.
New owned-stage preparation, filesystem capability/error handling, and retention
plumbing would add roughly **100–200 production lines** outside those
replacements. Allow roughly **150–300 test lines** for the new publication
cases, excluding platform integration infrastructure. These are scoping ranges,
not a promised net saving: enforcing exclusive ownership in the present same-UID
workspace has no implementation in this proposal, so its cost and safe
reclamation remain unestimated. A portable dual path may increase total code
instead.

Outside the four-file inventory, snapshot deletion also touches the depth helper
in `tests/branch_switch.rs` and `tests/reference.rs`'s
`clear_if_unchanged_with_test_hook` test. The latter is a shared-directory
cleanup test and must be retargeted or retained with that behavior. Worktree
rollback, index publication and HEAD publication remain separate call sites even
though their names do not contain `quarantine`.

## Compatibility constraints

Preserve the built
[Git authority contract](../spec/git-authority-threat-model.md). Prepared-target
integrity, retention of foreign data, and failure classification must not be
weakened by silently declaring the interleavings exercised by current tests out
of scope. Same-UID residual risk in the threat model does not establish
exclusive ownership of the current staging directory. The owner must accept the
specific enforceable writer boundary before snapshot deletion.

Keep exact blob bytes, literal filenames, staged-change preservation, supported
index flags, symlink/gitlink refusals, and existing bounds. A full-directory
replacement must account for unchanged, untracked and ignored content rather
than synthesizing only the target Git tree. Do not move or copy common Git
administration as part of a subtree exchange.

The risks requiring owner review are:

- **Loss of concurrent data:** exchange retains the displaced inode but later
  cleanup can destroy edits through old handles. Retention without a safe
  reclamation rule accumulates disk usage.
- **False publication evidence:** replacing the source name can publish an
  unprepared object even while its original descriptor remains open. A final
  metadata check merely moves this race.
- **Transaction boundaries:** readers opening the root once can follow one
  generation; independent path lookups can span generations. Index, HEAD and
  reflog still publish separately and can fail after the directory swap.
- **Authority and filesystem identity:** root exchange invalidates pinned root
  identities, mount boundaries can forbid rename, and native readers/writers do
  not automatically participate in daemon serialization.
- **Availability:** Linux NFS lacks the required flags on the inspected client;
  macOS volume capabilities and non-Linux fallback behavior need explicit
  acceptance. An ext4 success cannot justify enabling NFS.
- **Scale:** preparing larger parent subtrees adds streaming I/O and temporary
  disk proportional to the affected tree, possibly the whole checkout. It must
  not duplicate repository packs or allocate memory per blob byte. Measure wall
  time, scratch high-water usage and peak RSS on generated 1 GiB blobs and a
  checkout on the order of 1.7 GiB; retain the larger repository scale suite
  when integrating. No hard links to mutable source files may shortcut the copy.

## Acceptance criteria

### Existing test evidence

The tables describe source inspection, not a new test run. No existing test
proves that holding a directory FD excludes mutation or that a syscall
atomically publishes a whole checkout. The closest tests prove refusal/retention
at injected interleavings; their assertions constrain the replacement.

All names in this first table are in
[tests/branch_switch.rs](../../crates/tools-git/src/tests/branch_switch.rs).

| Test (starting line)                                                                                                                                                                                                                              | What it proves; disposition                                                                                                                                                         |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `branch_switch_preserves_a_prepared_target_replaced_before_publication` (409)                                                                                                                                                                     | Refuses substituted prepared leaf and retains foreign bytes. Retarget to the owned-source boundary; do not replace with only a successful exchange test.                            |
| `branch_switch_preserves_a_prepared_target_rewritten_before_publication` (465)                                                                                                                                                                    | Refuses in-place prepared-leaf mutation and retains bytes. Keep the property or prove that the permitted writer cannot perform the mutation.                                        |
| `failed_branch_switch_retains_every_obstructed_directory_quarantine` (569)                                                                                                                                                                        | Retains obstructed originals and restores unobstructed directories. Adapt to displaced-entry retention.                                                                             |
| `failed_branch_switch_preserves_a_foreign_entry_in_a_restored_quarantine` (660)                                                                                                                                                                   | Cleanup preserves an extra foreign entry during failed restoration. Keep recovery assertions.                                                                                       |
| `branch_switch_excludes_a_foreign_entry_from_source_quarantine_cleanup_capture` (738)                                                                                                                                                             | Foreign additions are not adopted as cleanup-owned contents; source restores. Replace snapshot hook with the corresponding ownership/publication boundary.                          |
| `quarantine_snapshot_failure_restores_the_original_directory` (799)                                                                                                                                                                               | Snapshot depth failure restores original data, including injected descendants. Replace snapshot-specific stimulus with preparation/publication failure; retain recovery assertions. |
| `successful_branch_switch_preserves_foreign_entries_in_both_quarantines` (857)                                                                                                                                                                    | Successful checkout retains foreign source/target entries and the retained source tree. Preserve; success does not authorize blind cleanup.                                         |
| `branch_switch_changes_real_head` (60), `branch_switch_checks_out_root_level_change` (273)                                                                                                                                                        | Final HEAD and checkout bytes change successfully. Keep; neither observes atomic publication.                                                                                       |
| `branch_switch_replaces_a_clean_tracked_file_with_a_directory` (311), `branch_switch_replaces_a_clean_tracked_directory_with_a_file` (362)                                                                                                        | Both type transitions work and ordinary cleanup completes. Keep both; container rename alone does not cover the second.                                                             |
| `branch_switch_preserves_a_modified_tracked_file_when_safe_checkout_refuses` (101)                                                                                                                                                                | Dirty tracked content survives refusal. Keep.                                                                                                                                       |
| `branch_switch_preserves_an_untracked_obstruction_when_safe_checkout_refuses` (131), `branch_switch_preserves_an_untracked_obstruction_in_a_target_ancestor` (174), `branch_switch_preserves_an_untracked_file_inside_a_replaced_directory` (519) | Untracked data survives obstruction/type transitions. Keep.                                                                                                                         |
| `branch_switch_checks_out_exact_blob_bytes_without_attribute_filtering` (979)                                                                                                                                                                     | Materialized bytes match the Git blob. Keep as preparation evidence.                                                                                                                |
| `branch_switch_preserves_a_nonconflicting_staged_change` (1194), `branch_switch_preserves_assume_valid_on_an_unchanged_path` (1239)                                                                                                               | Index semantics survive checkout. Keep independently of the directory operation.                                                                                                    |

The remaining branch-switch tests cover absent/symbolic branches, target
symlink/gitlink refusal, entry/tree/blob budgets, and changed staged or
skip-worktree paths. Keep them as admission/compatibility tests; they do not
prove publication atomicity.

In
[tests/regression_properties.rs](../../crates/tools-git/src/tests/regression_properties.rs):

| Test (starting line)                                                                                                                                                                                            | What it proves; disposition                                                                                                               |
| --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| `index_cleanup_retains_a_foreign_entry_when_restoration_is_blocked` (234)                                                                                                                                       | Both foreign quarantined index bytes and the obstructing live index survive. Keep unchanged; this is why index quarantine is not deleted. |
| `checkout_snapshot_treats_repository_paths_as_literal_filenames` (277)                                                                                                                                          | Preparation materializes `literal[1].txt` literally. Keep/retarget preparation; it does not observe publication.                          |
| `failed_unborn_commit_removes_its_new_reference_directories` (86), `reflog_rollback_removes_new_nested_hierarchy` (152)                                                                                         | Reference/reflog hierarchy rollback. Keep; unrelated to checkout directory exchange.                                                      |
| `object_publication_rejects_growth_beyond_the_live_database_budget` (30), `pinned_object_database_admits_a_well_formed_pack_within_the_aggregate_budget` (178), `log_rejects_a_nonempty_shallow_snapshot` (199) | Object/layout boundaries, not checkout publication. Keep.                                                                                 |

### New decisive regression

Add `checkout_publication_uses_one_owned_directory_generation` at the
publication primitive and exercise it through branch switch. Use barrier hooks,
not sleeps: prepare two files with distinguishable old/new bytes; pause
immediately before publication; keep old root and file descriptors open. After
one exchange, the published directory's identity must equal the retained
prepared descriptor and both files read through that directory must be the
prepared generation. The old descriptor must still reach the old tree. A reader
pins a generation once before reading its files; do not assert a snapshot across
independent path opens.

Include deterministic cases for a destination created just before no-replace
(expect refusal and untouched foreign bytes), source-name substitution, and
in-place writes through a pre-opened prepared-file descriptor. **An FD-only
implementation must fail this test**: under the proposed ownership model those
mutations must be prevented for the allowed writer, or publication must be
refused with all foreign bytes retained. Also write through an old live-file FD
after exchange and prove recovery/cleanup retains that edit. This distinguishes
namespace atomicity from both source integrity and safe reclamation.

Run the same directory operation cases on a Linux local filesystem and a macOS
volume supporting swap/exclusive rename, with an open directory and descendant.
On the actual NFS mount, record protocol, client/server versions, mount options,
and syscall results for disposable entries. Expect unsupported flagged
operations on the inspected Linux client; assert no live mutation and no unsafe
fallback. A future NFS backend needs its own cross-client interference and
uncertain-reply recovery evidence; this document does not authorize one.

Implementation acceptance additionally requires fault injection after directory
publication but before index/HEAD finalization, preservation of the existing
failure classifications, and the scale measurements above. The owner review must
settle staging exclusion, displaced-data reclamation, publication unit, and
unsupported-platform behavior before any deletion is implemented.
