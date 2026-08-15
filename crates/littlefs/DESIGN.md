# Snapshot filesystem design

## Scope

The first format is a bounded filesystem for small configuration, log, and
state files. It deliberately trades capacity and write amplification for a
small recovery state machine that can be exhaustively fault tested.

The implementation is `no_std`, does not allocate, uses explicit little-endian
codecs, and owns its block device through a generic type. No Rust struct is
written to flash directly.

## Device contract

The block device is NOR flash with:

- arbitrary byte reads;
- 4-byte aligned programming in multiples of 4 bytes;
- an 8 KiB erase block on the HC32F460;
- erased bytes equal to `0xff`;
- no programming from zero back to one;
- each 4-byte word programmed at most once after an erase;
- synchronous `sync`, or a barrier that makes prior writes durable.

The filesystem conservatively assumes that power loss may corrupt the current
4-byte program word or the entire block currently being erased. It does not
assume that a word program is atomic.

## Partition

The board integration reserves sectors 46 through 61:

```text
firmware       0x00000000 .. 0x0005bfff  (368 KiB)
filesystem     0x0005c000 .. 0x0007bfff  (16 x 8 KiB)
EFM self-test  0x0007c000 .. 0x0007dfff  (sector 62)
swap/reserved  0x0007e000 .. 0x0007ffff  (sector 63)
```

The linker script makes overlap with the filesystem a link-time error.

## Snapshot layout

Every committed state is one immutable circular segment. A segment begins at
an erase-block boundary and may wrap at the end of the partition.

```text
+----------------------+ offset 0
| 64-byte header       |
| - geometry/version   |
| - generation         |
| - payload len + CRC  |
| - header CRC         |
| - commit word LAST   |
+----------------------+ offset 64
| file/directory record|
| file/directory record|
| ...                  |
+----------------------+ payload_len
```

All integer fields are little-endian. Header CRC covers bytes `[0, 56)`. The
word at `[60, 64)` is never touched while the rest of the segment is written.
It is programmed once, as the final publish step. A mount candidate is accepted
only if all of these checks pass:

1. exact magic, format version, feature flags, geometry, and commit word;
2. header CRC and a canonical block span;
3. generation selected with wrapping serial-number comparison;
4. payload CRC, record boundaries, path rules, entry count, and uniqueness;
5. every file's independent data CRC;
6. directory records have no data, and every non-root parent exists as a
   directory record.

Each record contains a fixed 20-byte header followed immediately by its path and
file data, then zero padding to a four-byte record boundary. A directory is a
record with the directory flag, zero data length, and the CRC of empty data.
The root is implicit and consumes no record. Paths are canonical root-relative
UTF-8 strings no longer than 63 bytes. Payload length is therefore a multiple of
four and no program word is rewritten.

## Transaction order

For each mutation:

1. Keep the active segment unchanged.
2. Choose the circular run immediately after it. New segments are limited to
   half the partition, so this run cannot overlap the active segment.
3. Erase only the destination blocks.
4. Write the new header without its commit word and write the complete payload.
5. Call `sync`, read the candidate back, and verify all CRCs and structure.
6. Program the previously untouched commit word, call `sync`, and verify again.
7. Only then replace the in-RAM active descriptor.

An interrupted erase or program can damage only the inactive destination. If
the commit word is absent or torn, mount ignores it. If the commit word happens
to equal its exact value, the already-written header and payload still need to
pass independent CRC and structure validation. The old segment is never erased
before a complete new segment has been published.

After destination erase has begun, any transaction error marks the in-memory
filesystem as requiring remount. This handles the case where a device reports
failure after the commit word was physically programmed: the caller cannot
continue from an uncertain active generation. Read-only preflight errors such
as `NoSpace` do not poison the mount.

## Recovery and wear

Mount scans one header at each block boundary, fully validates every committed
candidate, and chooses the newest valid generation. It does not write recovery
state. Uncommitted and stale blocks are reclaimed only when they become the
destination of a later transaction.

### Erase-count wear leveling (format v1.1)

Every snapshot payload begins with a fixed per-block erase table: one
little-endian `u16` counter per block, padded to a program-unit boundary and
covered by the payload CRC. Counters are monotonic across reformats and
saturate at the `u16` width. This is the littlefs-style dynamic/static scheme,
adapted to a whole-snapshot copy-on-write filesystem:

- **Dynamic wear leveling**: each mutation computes its destination span and
  scans every non-overlapping start position, choosing the run with the lowest
  maximum erase count (lowest total, then smallest forward distance from the
  old successor, as tie-breaks). The tie-break reproduces the sequential sweep
  for uniformly worn devices, so uniform workloads keep rotating exactly as
  before. Because the active snapshot is fully rewritten by every mutation,
  dynamic placement alone converges the erase distribution even for workloads
  whose snapshot span alternates between one block and half the partition.
- **Static wear leveling**: `level()` rewrites the active snapshot unchanged
  into the least-worn non-overlapping run. It is an explicit, atomically
  committed relocation used to rebalance an idle filesystem whose blocks were
  worn unevenly by restricted large-span placements; it is a no-op (no device
  writes) when all counters are within one of each other.

The wear table travels inside each committed snapshot, so a torn relocation
mounts the previous complete snapshot with its previous counters; recovery is
unchanged and never depends on a separate table update transaction.

## Capacity and durability

The new and old snapshots must coexist. Therefore one snapshot may occupy no
more than half the blocks. With the board's sixteen-block partition, usable
serialized capacity is just under 64 KiB (the payload CRC covers the wear
table, which costs 2 bytes per block).

Every mutating API is a durability boundary. A successful return means the new
snapshot was synchronized and read back. On reset during an operation, mount
returns either the complete old version or the complete new version, never a
mixture. A device error requires remount before another mutation.

Renaming a directory rewrites its record and every descendant path in one
candidate snapshot. All destination lengths and namespace constraints are
checked before erase begins. It is never implemented as a sequence of child
renames, so recovery cannot expose a partially moved tree.

## Deliberate omissions

The format has no recursive removal, implicit parent creation, append log,
random overwrite, streaming file handle, sparse file, symbolic link,
permissions, timestamps, extended attributes, bad-block relocation, or
encryption. The erase counters are `u16` (saturating); a partition whose every
block reaches the saturation point is reported honestly by the counters rather
than remapped. CRC detects accidental corruption and torn writes; it is not a
security MAC.
