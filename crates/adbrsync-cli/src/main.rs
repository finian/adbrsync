mod args;
mod info;
mod progress;
mod report;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use adb_proto::{shell, AdbClient, DeviceSelector};
use adbrsync_core::perf::{DeviceFacts, OptionFacts, PerfReport, PhaseMillis};
use adbrsync_core::plan::DestState;
use adbrsync_core::{
    checksum, plan, scan, transfer, Digests, Filter, LocalEntry, PlanOptions, TransferOptions,
};
use anyhow::{bail, Context, Result};
use clap::Parser;

use crate::args::{parse_remote, parse_size, Args, RemoteSpec};
use crate::info::{InfoFlags, ParseOutcome};
use crate::report::Printer;

/// rsync's exit code for "some files could not be transferred".
const EXIT_PARTIAL: i32 = 23;

fn main() -> Result<()> {
    let args = Args::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let code = runtime.block_on(run(args))?;
    std::process::exit(code);
}

/// Resolve `--info`, `--progress` and `--stats` into one set of switches.
fn resolve_info(args: &Args, printer: &Printer) -> Option<InfoFlags> {
    let mut flags = InfoFlags {
        progress: u8::from(args.progress) * 2,
        stats: u8::from(args.stats),
        name: u8::from(args.verbose > 0),
    };
    let Some(spec) = &args.info else {
        return Some(flags);
    };
    match info::parse(spec) {
        ParseOutcome::Help => {
            println!("{}", info::HELP);
            None
        }
        ParseOutcome::Flags {
            flags: parsed,
            unknown,
            progress_promoted,
        } => {
            for name in unknown {
                printer.warn(&format!(
                    "unknown --info flag {name:?} ignored; try --info=help"
                ));
            }
            if progress_promoted {
                printer.warn(
                    "--info=progress1 treated as progress2: files move on many concurrent \
                     streams, so there is no single current file to show",
                );
            }
            // An explicit --info wins, but the older switches still turn things on.
            flags.progress = flags.progress.max(parsed.progress);
            flags.stats = flags.stats.max(parsed.stats);
            flags.name = flags.name.max(parsed.name);
            Some(flags)
        }
    }
}

/// Check the destinations and work out where the files actually land.
///
/// A destination is never created implicitly. On removable media an unmounted
/// drive leaves an empty mount point behind, and a backup written there fills
/// the internal disk while reporting success — the failure looks exactly like a
/// good run until the day the copy is needed.
fn resolve_destinations(
    args: &Args,
    source: &RemoteSpec,
    source_is_file: bool,
) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let mut roots = Vec::with_capacity(args.dests.len());
    let mut missing = Vec::new();

    for raw in &args.dests {
        if parse_remote(raw).is_some() {
            bail!("destination {raw:?} looks like a device path; this version only pulls");
        }
        let given = PathBuf::from(raw);
        match std::fs::metadata(&given) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => bail!("destination {raw:?} exists but is not a directory"),
            Err(_) if args.mkpath => std::fs::create_dir_all(&given)
                .with_context(|| format!("cannot create destination {raw}"))?,
            Err(_) => {
                missing.push(raw.clone());
                continue;
            }
        }
        roots.push(given);
    }

    if !missing.is_empty() && !args.skip_missing_dest {
        bail!(
            "destination does not exist: {}\n\
             A destination is never created implicitly. If this is a removable drive, \
             check that it is mounted: an unmounted drive leaves an empty mount point, \
             and writing there fills the internal disk instead. Pass --mkpath to create \
             it, or --skip-missing-dest to go ahead without it.",
            missing.join(", ")
        );
    }
    if roots.is_empty() {
        bail!("none of the destinations exist: {}", args.dests.join(", "));
    }

    // Two destinations that are the same directory, or one inside another,
    // would have the writes and the --delete pass fighting each other.
    let canonical: Vec<PathBuf> = roots
        .iter()
        .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
        .collect();
    for (i, a) in canonical.iter().enumerate() {
        for (j, b) in canonical.iter().enumerate() {
            if i == j {
                continue;
            }
            if a == b {
                bail!(
                    "destinations {} and {} are the same directory",
                    i + 1,
                    j + 1
                );
            }
            if a.starts_with(b) {
                bail!(
                    "destination {} ({}) is inside destination {} ({})",
                    i + 1,
                    a.display(),
                    j + 1,
                    b.display()
                );
            }
        }
    }

    // rsync's trailing-slash rule: without one, the source directory itself is
    // recreated inside each destination. A single file is the exception: it
    // always lands *in* the destination directory under its own name, so the
    // caller passes the directory either way.
    if !source.trailing_slash && !source_is_file {
        if let Some(name) = source.path.rsplit('/').next().filter(|n| !n.is_empty()) {
            for root in &mut roots {
                root.push(name);
            }
        }
    }
    Ok((roots, missing))
}

async fn run(args: Args) -> Result<i32> {
    let total_started = Instant::now();
    let printer = Printer::new(args.quiet, args.human_readable);
    let Some(info) = resolve_info(&args, &printer) else {
        return Ok(0); // --info=help
    };

    for (flag, set) in [("-p", args.perms), ("-o", args.owner), ("-g", args.group)] {
        if set {
            printer.warn(&format!(
                "{flag} ignored: the FUSE volume backing /sdcard synthesizes permissions, \
                 so they cannot be read from the device or restored"
            ));
        }
    }
    if args.compress {
        printer.warn(
            "-z ignored in this version: only brotli is negotiable on many devices and it \
             loses on already-compressed media",
        );
    }
    if args.partial {
        printer.warn(
            "--partial has no effect: the sync service cannot resume mid-file, so incomplete \
             files are always discarded",
        );
    }

    let source = parse_remote(&args.src).with_context(|| {
        format!(
            "source {:?} is not a device path like device:/sdcard/DCIM",
            args.src
        )
    })?;
    // One cheap round trip settles whether the source is a file, which decides
    // where the destination paths point before any scanning starts.
    let probe_client = AdbClient::new(args.server.clone());
    let probe_selector = match &source.serial {
        Some(serial) => DeviceSelector::Serial(serial.clone()),
        None => DeviceSelector::Any,
    };
    let source_is_file = match adb_proto::SyncSession::open(&probe_client, &probe_selector).await {
        Ok(mut sync) => sync
            .stat(&source.path)
            .await
            .map(|st| st.is_file())
            .unwrap_or(false),
        // A device we cannot reach is reported properly a few lines below.
        Err(_) => false,
    };
    let (dests, skipped) = resolve_destinations(&args, &source, source_is_file)?;
    for name in &skipped {
        printer.warn(&format!(
            "{name}: does not exist; skipped (--skip-missing-dest)"
        ));
    }

    let recursive = args.recursive || args.archive;
    let mut excludes = args.exclude.clone();
    if let Some(path) = &args.exclude_from {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read exclude file {path}"))?;
        excludes.extend(
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(str::to_string),
        );
    }
    let filter = Filter::new(&args.include, &excludes)?;

    let phase = Instant::now();
    let client = AdbClient::new(args.server.clone());
    let selector = match &source.serial {
        Some(serial) => DeviceSelector::Serial(serial.clone()),
        None => DeviceSelector::Any,
    };
    let device = client
        .resolve(&selector)
        .await
        .context("cannot select a device")?;
    if !device.is_usable() {
        bail!("device {} is in state {}", device.serial, device.state);
    }
    printer.info(&format!(
        "connected to {} ({})",
        device.serial,
        device.model.as_deref().unwrap_or("unknown model")
    ));
    // Pin the resolved serial so every later stream lands on the same device.
    let selector = DeviceSelector::Serial(device.serial.clone());
    let mut phases = PhaseMillis {
        connect: phase.elapsed().as_millis(),
        ..Default::default()
    };

    // The device walk and every local walk run at once. The destinations are
    // separate devices, so scanning all of them costs about what scanning one
    // does — which is what makes diffing each destination affordable.
    let phase = Instant::now();
    let remote_scan = scan::scan_remote(&client, &selector, &source.path);
    let local_scans: Vec<_> = dests
        .iter()
        .cloned()
        .map(|root| tokio::task::spawn_blocking(move || scan::scan_local(&root)))
        .collect();
    let remote = remote_scan.await.context("device scan failed")?;
    let mut locals: Vec<Vec<LocalEntry>> = Vec::with_capacity(dests.len());
    for (i, task) in local_scans.into_iter().enumerate() {
        let scanned = task
            .await
            .expect("local scan task")
            .with_context(|| format!("cannot scan destination {}", dests[i].display()))?;
        locals.push(scanned);
    }
    let mut remote = remote;
    phases.scan_remote = phase.elapsed().as_millis();

    if !recursive {
        // Without -r only the top level is considered, as in rsync.
        remote
            .entries
            .retain(|e| !e.rel.contains('/') && e.kind != adbrsync_core::EntryKind::Dir);
    }
    for denied in &remote.denied {
        printer.warn(&format!("skipped (permission denied): {denied}"));
    }

    let opts = PlanOptions {
        delete: args.delete,
        mtime_tolerance: args.mtime_tolerance,
        min_size: args.min_size.as_deref().and_then(parse_size),
        max_size: args.max_size.as_deref().and_then(parse_size),
        checksum: args.checksum,
    };

    let phase = Instant::now();
    let digests: Vec<Digests> = if args.checksum {
        printer.info("hashing files present on both sides");
        checksum::compare_many(&client, &selector, &remote.entries, &dests, &locals)
            .await
            .context("checksum comparison failed")?
    } else {
        dests.iter().map(|_| Digests::default()).collect()
    };
    phases.checksum = phase.elapsed().as_millis();

    let phase = Instant::now();
    let states: Vec<DestState> = dests
        .iter()
        .zip(locals.iter())
        .zip(digests.iter())
        .map(|((root, local), digests)| DestState {
            root: root.as_path(),
            local,
            digests,
        })
        .collect();
    let plan = plan::build(&remote, &states, &filter, &opts);
    drop(states);
    phases.plan = phase.elapsed().as_millis();

    printer.plan_summary(&remote, &plan);

    if args.dry_run || plan.is_empty() {
        if args.dry_run && info.wants_names() {
            printer.transfer_list(&plan, &dests);
        }
        phases.total = total_started.elapsed().as_millis();
        write_perf_report(
            &args,
            &client,
            &selector,
            &device,
            &dests,
            &excludes,
            recursive,
            args.streams.max(1),
            phases,
            &remote,
            &plan,
            &transfer::TransferReport::empty(),
            &printer,
        )
        .await?;
        printer.info(if args.dry_run {
            "dry run: nothing transferred"
        } else {
            "nothing to do"
        });
        return Ok(if skipped.is_empty() { 0 } else { EXIT_PARTIAL });
    }

    let stats = Arc::new(transfer::Stats::new(dests.len()));

    let phase = Instant::now();
    let unprepared = transfer::create_dirs(&plan, &dests)
        .await
        .context("cannot create destination directories")?;
    for (i, message) in &unprepared {
        printer.warn(&format!(
            "{}: cannot be written ({message}); continuing without it",
            dests[*i].display()
        ));
        stats.dests[*i].disable();
    }
    phases.create_dirs = phase.elapsed().as_millis();

    if info.wants_names() {
        printer.transfer_list(&plan, &dests);
    }
    let progress_task = info
        .wants_progress()
        .then(|| progress::spawn(Arc::clone(&stats)));

    let topts = TransferOptions {
        streams: args.streams.max(1),
        preserve_mtime: args.times || args.archive,
    };
    let phase = Instant::now();
    let mut transfer_report = transfer::run(
        &client,
        &selector,
        &plan,
        &dests,
        &topts,
        Arc::clone(&stats),
    )
    .await
    .context("transfer failed")?;
    phases.transfer = phase.elapsed().as_millis();

    if let Some(handle) = progress_task {
        handle.abort();
        progress::clear();
    }

    // A destination that was skipped or never got off the ground still has to
    // make the run count as a partial one, or the exit code would call it a
    // success.
    for name in &skipped {
        transfer_report.errors.push(transfer::FileError {
            rel: name.clone(),
            message: "destination does not exist; skipped".to_string(),
        });
    }
    for (i, message) in &unprepared {
        transfer_report.errors.push(transfer::FileError {
            rel: dests[i.to_owned()].display().to_string(),
            message: message.clone(),
        });
    }

    let phase = Instant::now();
    for (i, dest) in plan.dests.iter().enumerate() {
        if dest.deletions.is_empty() {
            continue;
        }
        let errors = transfer::apply_deletions(&dest.deletions).await;
        for e in &errors {
            printer.warn(&format!("cannot delete {}: {}", e.rel, e.message));
        }
        printer.info(&format!(
            "{}: deleted {} extraneous entries",
            dests[i].display(),
            dest.deletions.len() - errors.len()
        ));
        transfer_report.errors.extend(errors);
    }
    phases.delete = phase.elapsed().as_millis();
    phases.total = total_started.elapsed().as_millis();

    printer.final_report(&transfer_report, info.wants_stats());

    write_perf_report(
        &args,
        &client,
        &selector,
        &device,
        &dests,
        &excludes,
        recursive,
        topts.streams,
        phases,
        &remote,
        &plan,
        &transfer_report,
        &printer,
    )
    .await?;

    Ok(if transfer_report.errors.is_empty() {
        0
    } else {
        EXIT_PARTIAL
    })
}

/// Write the performance report, if one was asked for.
#[allow(clippy::too_many_arguments)]
async fn write_perf_report(
    args: &Args,
    client: &AdbClient,
    selector: &DeviceSelector,
    device: &adb_proto::DeviceInfo,
    dests: &[PathBuf],
    excludes: &[String],
    recursive: bool,
    streams: usize,
    phases: PhaseMillis,
    remote: &scan::RemoteScan,
    plan: &adbrsync_core::Plan,
    transfer_report: &transfer::TransferReport,
    printer: &Printer,
) -> Result<()> {
    let Some(path) = &args.perf_report else {
        return Ok(());
    };
    let options = OptionFacts {
        streams,
        recursive,
        checksum: args.checksum,
        delete: args.delete,
        preserve_mtime: args.times || args.archive,
        mtime_tolerance: args.mtime_tolerance,
        excludes: excludes.len(),
        includes: args.include.len(),
    };
    let report = PerfReport::build(
        &args.src,
        dests,
        device_facts(client, selector, device).await,
        options,
        phases,
        remote,
        plan,
        transfer_report,
    );
    let path = Path::new(path);
    report
        .write_to(path)
        .with_context(|| format!("cannot write performance report to {}", path.display()))?;
    printer.info(&format!("performance report written to {}", path.display()));
    Ok(())
}

/// Collect device details for the performance report. Best effort: a missing
/// property must not fail a transfer that already succeeded.
async fn device_facts(
    client: &AdbClient,
    selector: &DeviceSelector,
    device: &adb_proto::DeviceInfo,
) -> DeviceFacts {
    let props = shell::run(
        client,
        selector,
        "getprop ro.build.version.release; getprop ro.build.version.sdk; \
         getprop ro.product.cpu.abi",
    )
    .await
    .ok();
    let mut lines = props
        .as_ref()
        .map(|o| o.stdout_text().lines().map(str::to_string).collect())
        .unwrap_or_else(Vec::new)
        .into_iter();

    DeviceFacts {
        serial: device.serial.clone(),
        model: device.model.clone(),
        android_release: lines.next(),
        sdk: lines.next(),
        abi: lines.next(),
        adb_server_version: client.server_version().await.ok(),
        features: client
            .features(selector)
            .await
            .map(|f| f.into_iter().collect())
            .unwrap_or_default(),
    }
}
