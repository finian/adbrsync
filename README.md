# adbrsync

A high-throughput Android backup engine in Rust, transferring over the ADB
transport with an interface that follows `rsync` where Android permits.

Status: **v0.1, pull only** (device → host). See [Scope](#scope).

## Why it is fast

The bottleneck in a real backup is round trips, not bandwidth. A phone's photo
and app-data tree is bimodal: on the reference device 71.5% of files are under
64 KiB but together hold 1.0% of the bytes, while 5.4% of files hold 90.7%.
Anything that pays a fixed cost per file loses badly on the first group.

Three things follow from that, and they are the whole design:

**The `adb` binary is never spawned.** `adbrsync` speaks the adb server protocol
directly. Spawning `adb` costs ~63 ms per invocation on the reference device,
against ~4 ms for a protocol round trip. Tools that shell out to `adb pull`
per file pay the former on every one of them.

**Transfers run on many concurrent sync streams.** A single sync stream cannot
have two requests in flight — responses carry no request tag — so concurrency
comes from opening several connections to the adb server. Measured on 617 small
files: 15.6 s on one stream, 6.4 s on sixteen.

**The device tree is walked in one round trip.** A single `find -printf` returns
the whole tree; recursive per-directory listing would pay the same device-side
stat cost plus one round trip per directory (6,259 of them on the reference
device). Scanning 23,110 entries takes ~3.1 s, of which 89% is the device's own
filesystem walk — close to the floor for an unrooted device.

## Install

Requires a Rust toolchain and the `adb` server on the host.

```
cargo build --release
```

The binary lands at `target/release/adbrsync`.

## Usage

```
adbrsync [OPTIONS] SRC DEST
```

`SRC` names a device path as `device:/path` (any single connected device) or
`<serial>:/path`. `DEST` is a local directory. A trailing slash on `SRC` copies
the directory's contents; without one the directory itself is created inside
`DEST` — the same rule as rsync.

```sh
# Mirror the camera roll, removing local files that are gone from the device
adbrsync -av --delete device:/sdcard/DCIM/ ./backup/

# Preview without transferring
adbrsync -avn device:/sdcard/ ./backup/

# A specific device, skipping caches
adbrsync -av --exclude '*.tmp' --exclude 'Android/data/**' \
  192.168.0.108:41567:/sdcard/ ./backup/

# Raise concurrency and show progress
adbrsync -a --streams 24 --progress --stats device:/sdcard/Pictures/ ./backup/
```

## rsync compatibility

**Supported:** `-a` `-r` `-t` `-l` `-v` `-q` `-n/--dry-run` `--delete`
`--exclude` `--exclude-from` `--include` `--progress` `--info=FLAGS` `--stats`
`-c/--checksum` `--max-size` `--min-size` `-h/--human-readable`.

**Accepted and ignored, with a warning:** `-p` `-o` `-g` (see below), `-z`
(only brotli is negotiable on many devices, and it loses on already-compressed
media), `--partial` (the sync service cannot resume mid-file).

Three deviations worth knowing: `--include` is consulted before `--exclude`
rather than resolving both in the order written; directory modification times
are not restored (file times are); and `--info=progress1` is treated as
`progress2`, because files move on many concurrent streams at once and there is
no single "current file" to draw a per-file bar for. `--info=help` lists what is
recognised.

**Not implemented:** the delta algorithm, `-H` hard links, `-A` ACLs,
`-X` xattrs, `--link-dest`, daemon mode, ssh transport, and pushing to the
device.

Additional options: `--streams N` (concurrent sync streams, default 16),
`--server ADDR`, `--mtime-tolerance SECONDS`, `--perf-report FILE`.

Note that `-h` is human-readable output, as in rsync; use `--help` for usage.

## Performance reports

`--perf-report FILE` writes a JSON record of the run, so tuning decisions can be
argued from data. It is written even for `--dry-run` and for runs that transfer
nothing, because the tree scan is often the slow part and worth measuring on its
own.

```sh
adbrsync -a --info=progress2 --perf-report run.json device:/sdcard/ ./backup/
```

The record holds per-phase wall times (scan, plan, checksum, transfer, delete),
the negotiated device facts, a per-size-class breakdown of where transfer time
went, a per-second completion timeline, and every raw per-file sample as
`[size_bytes, duration_ms, start_ms]`.

The size classes are usually the first thing to read: they show throughput as a
function of file size, which is where the difference between "bound by the link"
and "bound by per-file cost" becomes visible.

## Android caveats

These are properties of the platform, not omissions. `adbrsync` states them
rather than pretending otherwise:

- **`-a` means `-rt` here.** The FUSE mount behind `/sdcard` synthesizes
  permissions, owner and group. They cannot be read as real values or restored,
  so `-p -o -g` are refused rather than silently faked.
- **Whole-file transfer is always used.** rsync's rolling checksum would force a
  full device-side read of every candidate file to save transfers that do not
  happen on media-dominated backup sets.
- **App-private data may be unreadable.** On Android 11+ the adb shell user
  generally cannot read `/sdcard/Android/data` or `/Android/obb`. Those paths
  are reported as skipped; a partial backup never reports as a clean one. Some
  devices mount them readably and are unaffected.
- **Mid-file resume is impossible.** Neither `RECV` nor `RECV_V2` accepts a byte
  offset. Interrupted files are written to a temporary name and discarded, so a
  truncated file is never left looking complete.

## Measured behaviour

Reference device: OnePlus 6T, Android 11, unrooted, over **wireless debugging**.
Link-dependent figures do not carry over to USB; device-side figures do.

| | |
|---|---|
| Tree scan, 23,110 entries | 3.1 s (89% device-side filesystem walk) |
| Device-side FUSE read | 437 MB/s — ~12× a USB 2.0 link, so not the bottleneck |
| 617 files / 64 MiB, 1 stream | 15.6 s |
| 617 files / 64 MiB, 16 streams | 6.4 s |
| Same set via `adb pull` | 23.2 s |
| Re-run with nothing changed | 1.5 s |
| Whole device: 16,852 files / 11.69 GiB | 880 s, 13.60 MB/s |

Concurrency scales steeply to 16 streams and falls off slowly past it, which
sets the default. Re-tune with `--streams` over USB.

The mixed whole-device run above moved 13.60 MB/s. A run of only large files at
the same concurrency — where per-file cost is negligible by construction —
managed 12.86 MB/s. Fifty times as many files, and throughput did not drop:
per-file overhead is fully hidden behind concurrency on a link this speed.

## Scope

v0.1 pulls from the device. Pushing, the delta algorithm, and Linux host
support are deliberately deferred.

An on-device agent — pushing a helper binary to batch many files into one
stream — was designed, costed, and then dropped on the measurement above: with
concurrency applied there is no per-file overhead left to recover. That holds
while the link is the bottleneck. On a much faster link it would not, so
`--stats` reports the per-file fixed cost and how busy the streams were, and
says plainly when a run had too few small files to measure it rather than
guessing.

## License

MIT. See [LICENSE](LICENSE).
