//! Benchmark: `cargo run --release --example scan_bench -- [--walk] <drive letter or folder>`.
//! Prints the scan mode, elapsed seconds, file count and total size on disk.

use dirstat::model::Tree;
use dirstat::scan::{self, ScanEvent, ScanMode, ScanTarget};
use dirstat::ui_tree::format_size;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::channel;
use std::time::Instant;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let force_walk = args.iter().any(|arg| arg == "--walk");
    let Some(arg) = args.iter().find(|arg| *arg != "--walk") else {
        eprintln!("usage: scan_bench [--walk] <drive letter or folder>");
        return ExitCode::FAILURE;
    };
    let target = parse_target(arg);

    let started = Instant::now();
    let (mode, tree) = if force_walk {
        let root = match &target {
            ScanTarget::Volume(letter) => PathBuf::from(format!(r"{letter}:\")),
            ScanTarget::Folder(path) => path.clone(),
        };
        let (tree, _skipped) = scan::walk::walk(&root, &AtomicBool::new(false), &mut |_, _| {});
        (ScanMode::Walk, tree)
    } else {
        let (tx, rx) = channel();
        let _handle = scan::start(target.clone(), tx, Box::new(|| {}));
        loop {
            match rx.recv() {
                Ok(ScanEvent::Done { tree, mode, .. }) => break (mode, tree),
                Ok(ScanEvent::Failed(message)) => {
                    eprintln!("scan failed: {message}");
                    return ExitCode::FAILURE;
                }
                Ok(ScanEvent::Progress { .. }) => {}
                Err(_) => {
                    eprintln!("scan ended without a result");
                    return ExitCode::FAILURE;
                }
            }
        }
    };
    let elapsed = started.elapsed().as_secs_f64();

    println!("{}", mode_line(mode, force_walk));
    println!("target:        {}", target_text(&target));
    println!("elapsed:       {elapsed:.2} s");
    println!("files:         {}", file_count(&tree));
    println!("size on disk:  {}", format_size(tree.nodes[tree.root.0 as usize].size));
    ExitCode::SUCCESS
}

/// A single letter, optionally followed by a colon, is a volume; anything else is a folder.
fn parse_target(arg: &str) -> ScanTarget {
    let letter = arg.strip_suffix(':').unwrap_or(arg);
    let mut chars = letter.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => ScanTarget::Volume(c.to_ascii_uppercase()),
        _ => ScanTarget::Folder(PathBuf::from(arg)),
    }
}

fn target_text(target: &ScanTarget) -> String {
    match target {
        ScanTarget::Volume(letter) => format!("{letter}:"),
        ScanTarget::Folder(path) => path.display().to_string(),
    }
}

fn mode_line(mode: ScanMode, forced: bool) -> String {
    match (mode, forced) {
        (_, true) => "mode:          Walk (forced by --walk)".to_string(),
        (ScanMode::Mft, false) => "mode:          Mft".to_string(),
        (ScanMode::Walk, false) => {
            "mode:          Walk (MFT path not used: not elevated, or not an NTFS volume)".to_string()
        }
    }
}

fn file_count(tree: &Tree) -> usize {
    tree.nodes.iter().filter(|node| !node.is_dir).count()
}
