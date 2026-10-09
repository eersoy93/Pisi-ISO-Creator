//! Pisi ISO Creator: build and edit Pisi GNU/Linux live ISO images from
//! PiSi packages, with a Slint user interface and a command line mode.

mod command;
mod gui;
mod pipeline;
mod pisi;
mod project;

use command::{Event, EventSink, Executor};
use project::{EditJob, Project};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const USAGE: &str = "\
Pisi ISO Creator - create and edit Pisi GNU/Linux ISO images

Usage:
  pisi-iso-creator                          Start the graphical interface
  pisi-iso-creator new <project.toml>       Write a default project file
  pisi-iso-creator build <project.toml> [--dry-run | --plan]
  pisi-iso-creator edit <edit-job.toml> [--dry-run | --plan]
  pisi-iso-creator search <pisi-index.xml[.xz] URL or path> [query]
  pisi-iso-creator --help

Options:
  --dry-run   Log every step without executing anything
  --plan      Print the build plan as a shell-like script and exit

Building and editing images requires root privileges and the PiSi
package manager, squashfs-tools, xorriso, mtools and GRUB on the host.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        return match gui::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("Error: Can not start the user interface: {e}");
                ExitCode::FAILURE
            }
        };
    }
    match run_cli(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_cli(args: &[String]) -> Result<(), String> {
    let flags: Vec<&str> = args
        .iter()
        .filter(|a| a.starts_with("--"))
        .map(String::as_str)
        .collect();
    let positional: Vec<&str> = args
        .iter()
        .filter(|a| !a.starts_with("--"))
        .map(String::as_str)
        .collect();
    if flags.contains(&"--help") || positional.first() == Some(&"help") {
        println!("{USAGE}");
        return Ok(());
    }
    if let Some(unknown) = flags
        .iter()
        .find(|f| !matches!(**f, "--dry-run" | "--plan"))
    {
        return Err(format!("Unknown option: {unknown}\n\n{USAGE}"));
    }
    let dry_run = flags.contains(&"--dry-run");
    let plan_only = flags.contains(&"--plan");

    let plan = match positional.as_slice() {
        ["new", path] => {
            let path = PathBuf::from(path);
            if path.exists() {
                return Err(format!("{} already exists!", path.display()));
            }
            Project::default().save(&path)?;
            println!("Wrote {}", path.display());
            return Ok(());
        }
        ["build", path] => {
            let project = Project::load(&PathBuf::from(path))?;
            check(project.validate())?;
            pipeline::build_plan(&project)
        }
        ["search", source, query @ ..] if query.len() <= 1 => {
            let index = pisi::load_index(source)?;
            let found = index.search(query.first().copied().unwrap_or_default());
            let mut out = std::io::stdout().lock();
            for package in &found {
                if writeln!(out, "{}", package.label()).is_err() {
                    break;
                }
            }
            eprintln!(
                "{} of {} packages, {} components",
                found.len(),
                index.packages.len(),
                index.components.len()
            );
            return Ok(());
        }
        ["edit", path] => {
            let job = EditJob::load(&PathBuf::from(path))?;
            check(job.validate())?;
            pipeline::edit_plan(&job)
        }
        _ => return Err(format!("Invalid arguments!\n\n{USAGE}")),
    };

    if plan_only {
        print!("{}", plan.describe());
        return Ok(());
    }
    let sink: EventSink = Arc::new(|event| match event {
        Event::Log(line) => println!("{line}"),
        Event::Progress { .. } | Event::Finished(_) => {}
    });
    Executor::new(dry_run, Arc::new(AtomicBool::new(false)), sink).execute(&plan)
}

fn check(errors: Vec<String>) -> Result<(), String> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!("Invalid configuration:\n  {}", errors.join("\n  ")))
    }
}
