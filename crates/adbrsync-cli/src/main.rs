mod args;
mod info;
mod progress;
mod report;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use adb_proto::{shell, AdbClient, DeviceSelector};
use adbrsync_core::perf::{DeviceFacts, OptionFacts, PerfReport, PhaseMillis};
use adbrsync_core::{
    checksum, plan, scan, transfer, Digests, Filter, PlanOptions, TransferOptions,
};
use anyhow::{bail, Context, Result};
use clap::Parser;

use crate::args::{parse_remote, parse_size, Args};
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
    if parse_remote(&args.dest).is_some() {
        bail!("destination must be a local path; this version only pulls from the device");
    }

    // rsync's trailing-slash rule: without one, the source directory itself is
    // recreated inside the destination.
    let mut dest = PathBuf::from(&args.dest);
    if !source.trailing_slash {
        if let Some(name) = source.path.rsplit('/').next().filter(|n| !n.is_empty()) {
            dest.push(name);
        }
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

    // Both trees are walked at once; neither blocks the other.
    let phase = Instant::now();
    let remote_scan = scan::scan_remote(&client, &selector, &source.path);
    let dest_for_scan = dest.clone();
    let local_scan = tokio::task::spawn_blocking(move || scan::scan_local(&dest_for_scan));
    let (remote, local) = tokio::join!(remote_scan, local_scan);
    let mut remote = remote.context("device scan failed")?;
    let local = local.expect("local scan task")?;
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
    let digests = if args.checksum {
        printer.info("hashing files present on both sides");
        checksum::compare(&client, &selector, &remote.entries, &local, &dest)
            .await
            .context("checksum comparison failed")?
    } else {
        Digests::default()
    };
    phases.checksum = phase.elapsed().as_millis();

    let phase = Instant::now();
    let plan = plan::build(&remote, &local, &dest, &filter, &opts, &digests);
    phases.plan = phase.elapsed().as_millis();

    printer.plan_summary(&remote, &plan);

    // A run that transfers nothing still measured a scan, and that is often the
    // slow part; write the report rather than silently skipping it.
    if args.dry_run || plan.is_empty() {
        if args.dry_run && info.wants_names() {
            printer.transfer_list(&plan);
        }
        phases.total = total_started.elapsed().as_millis();
        write_perf_report(
            &args,
            &client,
            &selector,
            &device,
            &dest,
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
        return Ok(0);
    }

    let phase = Instant::now();
    transfer::create_dirs(&plan, &dest)
        .await
        .context("cannot create destination directories")?;
    phases.create_dirs = phase.elapsed().as_millis();

    if info.wants_names() {
        printer.transfer_list(&plan);
    }

    let stats = Arc::new(transfer::Stats::default());
    let progress_task = info
        .wants_progress()
        .then(|| progress::spawn(Arc::clone(&stats)));

    let topts = TransferOptions {
        streams: args.streams.max(1),
        preserve_mtime: args.times || args.archive,
    };
    let phase = Instant::now();
    let mut transfer_report =
        transfer::run(&client, &selector, &plan, &dest, &topts, Arc::clone(&stats))
            .await
            .context("transfer failed")?;
    phases.transfer = phase.elapsed().as_millis();

    if let Some(handle) = progress_task {
        handle.abort();
        progress::clear();
    }

    let phase = Instant::now();
    if !plan.deletions.is_empty() {
        let errors = transfer::apply_deletions(&plan.deletions).await;
        for e in &errors {
            printer.warn(&format!("cannot delete {}: {}", e.rel, e.message));
        }
        printer.info(&format!(
            "deleted {} extraneous entries",
            plan.deletions.len()
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
        &dest,
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
    dest: &std::path::Path,
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
        dest,
        device_facts(client, selector, device).await,
        options,
        phases,
        remote,
        plan,
        transfer_report,
    );
    let path = PathBuf::from(path);
    report
        .write_to(&path)
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
