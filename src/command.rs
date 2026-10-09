//! External commands and the step executor used by the ISO creator and
//! editor pipelines.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// An external program invocation. Arguments are passed directly to the
/// program (never through a shell).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cmd {
    pub program: OsString,
    pub args: Vec<OsString>,
    /// A non-zero exit status is logged but does not abort the pipeline.
    pub allow_failure: bool,
}

impl Cmd {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_owned(),
            args: Vec::new(),
            allow_failure: false,
        }
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self
    }

    pub fn allow_failure(mut self) -> Self {
        self.allow_failure = true;
        self
    }
}

impl fmt::Display for Cmd {
    /// Shell-quoted representation, for logs and dry runs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", shell_quote(&self.program.to_string_lossy()))?;
        for arg in &self.args {
            write!(f, " {}", shell_quote(&arg.to_string_lossy()))?;
        }
        Ok(())
    }
}

pub fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// One unit of work of a pipeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Run(Cmd),
    /// `mkdir -p`
    CreateDir(PathBuf),
    /// Creates a mount point below `root`, refusing paths that pass through
    /// symbolic links (which could redirect a mount onto the host).
    CreateMountPoint {
        root: PathBuf,
        path: PathBuf,
    },
    /// Removes a directory tree if it exists. Refuses to follow active mounts.
    RemoveDir(PathBuf),
    WriteFile {
        path: PathBuf,
        contents: String,
    },
    /// Waits until a file (e.g. a socket) exists.
    WaitForFile {
        path: PathBuf,
        timeout_secs: u64,
    },
    /// Unmounts a path if (and only if) it is currently a mount point.
    Unmount(PathBuf),
    /// Copies the newest kernel (`/boot/kernel-*` or `/boot/vmlinuz-*`) and
    /// its initramfs from a root file system to `dest/kernel` and
    /// `dest/initrd`. `initrd` overrides the initramfs path in the rootfs.
    CopyBootFiles {
        rootfs: PathBuf,
        dest: PathBuf,
        initrd: Option<PathBuf>,
    },
    /// Locates the root file system SquashFS image inside an extracted ISO
    /// tree and unpacks it to `rootfs`.
    UnpackSquashfs {
        iso_dir: PathBuf,
        rootfs: PathBuf,
    },
    /// Rebuilds the SquashFS image found by [`Action::UnpackSquashfs`].
    RepackSquashfs {
        rootfs: PathBuf,
        compression: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub description: String,
    pub action: Action,
}

impl Step {
    pub fn new(description: impl Into<String>, action: Action) -> Self {
        Self {
            description: description.into(),
            action,
        }
    }

    pub fn run(description: impl Into<String>, cmd: Cmd) -> Self {
        Self::new(description, Action::Run(cmd))
    }
}

/// An ordered list of steps plus cleanup steps that are always executed
/// (also after failures or cancellation), e.g. unmounting.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub title: String,
    pub required_tools: Vec<&'static str>,
    pub steps: Vec<Step>,
    pub cleanup: Vec<Step>,
}

impl Plan {
    /// Human readable, shell-like listing of the plan.
    pub fn describe(&self) -> String {
        let mut out = format!("# {}\n", self.title);
        for (i, step) in self.steps.iter().enumerate() {
            out.push_str(&format!(
                "\n# [{}/{}] {}\n",
                i + 1,
                self.steps.len(),
                step.description
            ));
            out.push_str(&describe_action(&step.action));
            out.push('\n');
        }
        if !self.cleanup.is_empty() {
            out.push_str("\n# Cleanup\n");
            for step in &self.cleanup {
                out.push_str(&describe_action(&step.action));
                out.push('\n');
            }
        }
        out
    }
}

fn describe_action(action: &Action) -> String {
    match action {
        Action::Run(cmd) if cmd.allow_failure => format!("{cmd} || true"),
        Action::Run(cmd) => cmd.to_string(),
        Action::CreateDir(p) | Action::CreateMountPoint { path: p, .. } => {
            format!("mkdir -p {}", quote_path(p))
        }
        Action::RemoveDir(p) => format!("rm -rf --one-file-system {}", quote_path(p)),
        Action::WriteFile { path, contents } => format!(
            "cat > {} <<'EOF'\n{}{}EOF",
            quote_path(path),
            contents,
            if contents.ends_with('\n') { "" } else { "\n" }
        ),
        Action::WaitForFile { path, timeout_secs } => {
            format!("# wait up to {timeout_secs}s for {}", quote_path(path))
        }
        Action::Unmount(p) => format!("mountpoint -q {0} && umount -l {0}", quote_path(p)),
        Action::CopyBootFiles {
            rootfs,
            dest,
            initrd,
        } => format!(
            "cp <newest {}/boot/kernel-*> {}/kernel\ncp {} {}/initrd",
            rootfs.display(),
            dest.display(),
            initrd
                .as_ref()
                .map(|i| quote_path(&rootfs.join(i.strip_prefix("/").unwrap_or(i))))
                .unwrap_or_else(|| "<matching initramfs>".into()),
            dest.display()
        ),
        Action::UnpackSquashfs { iso_dir, rootfs } => format!(
            "unsquashfs -f -d {} <root SquashFS image found in {}>",
            quote_path(rootfs),
            quote_path(iso_dir)
        ),
        Action::RepackSquashfs {
            rootfs,
            compression,
        } => format!(
            "mksquashfs {} <original image path> -noappend -comp {compression}",
            quote_path(rootfs)
        ),
    }
}

fn quote_path(p: &Path) -> String {
    shell_quote(&p.to_string_lossy())
}

/// Events emitted while a plan is executed.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Log(String),
    Progress {
        step: usize,
        total: usize,
        description: String,
    },
    Finished(Result<(), String>),
}

pub type EventSink = Arc<dyn Fn(Event) + Send + Sync>;

/// Mutable state shared between steps.
#[derive(Default)]
struct Context {
    squashfs_image: Option<PathBuf>,
}

pub struct Executor {
    pub dry_run: bool,
    pub cancel: Arc<AtomicBool>,
    sink: EventSink,
}

impl Executor {
    pub fn new(dry_run: bool, cancel: Arc<AtomicBool>, sink: EventSink) -> Self {
        Self {
            dry_run,
            cancel,
            sink,
        }
    }

    fn log(&self, msg: impl Into<String>) {
        (self.sink)(Event::Log(msg.into()));
    }

    /// Executes the plan, always followed by its cleanup steps. Emits a
    /// final [`Event::Finished`] and returns the same result.
    pub fn execute(&self, plan: &Plan) -> Result<(), String> {
        let result = self.execute_inner(plan);
        if !plan.cleanup.is_empty() {
            self.log("==> Cleanup");
            let mut ctx = Context::default();
            for step in &plan.cleanup {
                if let Err(e) = self.perform(&step.action, &mut ctx) {
                    self.log(format!("Warning: Cleanup step failed: {e}"));
                }
            }
        }
        match &result {
            Ok(()) => self.log(format!("==> {} finished successfully", plan.title)),
            Err(e) => self.log(format!("==> {} failed: {e}", plan.title)),
        }
        (self.sink)(Event::Finished(result.clone()));
        result
    }

    fn execute_inner(&self, plan: &Plan) -> Result<(), String> {
        self.log(format!(
            "==> {}{}",
            plan.title,
            if self.dry_run { " (dry run)" } else { "" }
        ));
        if !self.dry_run {
            let missing = missing_tools(&plan.required_tools);
            if !missing.is_empty() {
                return Err(format!(
                    "Required tools not found in PATH: {} !",
                    missing.join(", ")
                ));
            }
            if !is_root() {
                return Err("Building and editing ISO images requires root privileges!".into());
            }
        }
        let mut ctx = Context::default();
        let total = plan.steps.len();
        for (i, step) in plan.steps.iter().enumerate() {
            if self.cancel.load(Ordering::SeqCst) {
                return Err("Cancelled by user!".into());
            }
            (self.sink)(Event::Progress {
                step: i,
                total,
                description: step.description.clone(),
            });
            self.log(format!("==> [{}/{}] {}", i + 1, total, step.description));
            self.perform(&step.action, &mut ctx)
                .map_err(|e| format!("{}: {e}", step.description))?;
        }
        (self.sink)(Event::Progress {
            step: total,
            total,
            description: "Done".into(),
        });
        Ok(())
    }

    fn perform(&self, action: &Action, ctx: &mut Context) -> Result<(), String> {
        if self.dry_run {
            for line in describe_action(action).lines() {
                self.log(format!("    {line}"));
            }
            return Ok(());
        }
        match action {
            Action::Run(cmd) => {
                self.log(format!("$ {cmd}"));
                match self.run_command(cmd) {
                    Err(e) if cmd.allow_failure => {
                        self.log(format!("Warning: Ignoring failure: {e}"));
                        Ok(())
                    }
                    other => other,
                }
            }
            Action::CreateDir(p) => {
                fs::create_dir_all(p).map_err(|e| format!("Can not create {}: {e} !", p.display()))
            }
            Action::CreateMountPoint { root, path } => create_mount_point(root, path),
            Action::RemoveDir(p) => remove_dir(p),
            Action::WriteFile { path, contents } => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| format!("Can not create {}: {e} !", parent.display()))?;
                }
                fs::write(path, contents)
                    .map_err(|e| format!("Can not write {}: {e} !", path.display()))
            }
            Action::WaitForFile { path, timeout_secs } => {
                let start = Instant::now();
                while !path.exists() {
                    if start.elapsed() > Duration::from_secs(*timeout_secs) {
                        return Err(format!("Timed out waiting for {} !", path.display()));
                    }
                    if self.cancel.load(Ordering::SeqCst) {
                        return Err("Cancelled by user!".into());
                    }
                    thread::sleep(Duration::from_millis(200));
                }
                Ok(())
            }
            Action::Unmount(p) => {
                if is_mount_point(p) {
                    self.log(format!("$ umount -l {}", p.display()));
                    self.run_command(&Cmd::new("umount").arg("-l").arg(p))
                } else {
                    Ok(())
                }
            }
            Action::CopyBootFiles {
                rootfs,
                dest,
                initrd,
            } => {
                let (kernel, initramfs) = find_boot_files(rootfs, initrd.as_deref())?;
                fs::create_dir_all(dest)
                    .map_err(|e| format!("Can not create {}: {e} !", dest.display()))?;
                for (src, name) in [(kernel, "kernel"), (initramfs, "initrd")] {
                    self.log(format!(
                        "Copy {} -> {}/{name}",
                        src.display(),
                        dest.display()
                    ));
                    fs::copy(&src, dest.join(name))
                        .map_err(|e| format!("Can not copy {}: {e} !", src.display()))?;
                }
                Ok(())
            }
            Action::UnpackSquashfs { iso_dir, rootfs } => {
                let image = find_squashfs(iso_dir)?;
                self.log(format!("Found root file system image {}", image.display()));
                ctx.squashfs_image = Some(image.clone());
                let cmd = Cmd::new("unsquashfs")
                    .arg("-f")
                    .arg("-d")
                    .arg(rootfs)
                    .arg(&image);
                self.log(format!("$ {cmd}"));
                self.run_command(&cmd)
            }
            Action::RepackSquashfs {
                rootfs,
                compression,
            } => {
                let image = ctx
                    .squashfs_image
                    .clone()
                    .ok_or("No SquashFS image was unpacked!")?;
                let cmd = mksquashfs(rootfs, &image, compression);
                self.log(format!("$ {cmd}"));
                self.run_command(&cmd)
            }
        }
    }

    /// Runs a command, streaming its stdout and stderr to the log. The
    /// process is killed when the run is cancelled.
    fn run_command(&self, cmd: &Cmd) -> Result<(), String> {
        let mut child = Command::new(&cmd.program)
            .args(&cmd.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Can not start {}: {e} !", cmd.program.to_string_lossy()))?;

        let readers: Vec<_> = [
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        ]
        .into_iter()
        .flatten()
        .map(|stream| {
            let sink = self.sink.clone();
            thread::spawn(move || {
                for line in BufReader::new(stream).split(b'\n').map_while(Result::ok) {
                    let line = String::from_utf8_lossy(&line);
                    // Progress bars use carriage returns; keep the last state.
                    let line = line
                        .rsplit('\r')
                        .find(|s| !s.trim().is_empty())
                        .unwrap_or("");
                    sink(Event::Log(line.trim_end().to_string()));
                }
            })
        })
        .collect();

        let status = loop {
            if let Some(status) = child
                .try_wait()
                .map_err(|e| format!("Can not wait for {}: {e} !", cmd.program.to_string_lossy()))?
            {
                break status;
            }
            if self.cancel.load(Ordering::SeqCst) {
                let _ = child.kill();
                let _ = child.wait();
                for r in readers {
                    let _ = r.join();
                }
                return Err("Cancelled by user!".into());
            }
            thread::sleep(Duration::from_millis(100));
        };
        for r in readers {
            let _ = r.join();
        }
        if status.success() {
            Ok(())
        } else {
            Err(format!(
                "{} exited with {status} !",
                cmd.program.to_string_lossy()
            ))
        }
    }
}

pub fn mksquashfs(rootfs: &Path, image: &Path, compression: &str) -> Cmd {
    let cmd = Cmd::new("mksquashfs")
        .arg(rootfs)
        .arg(image)
        .arg("-noappend")
        .arg("-comp")
        .arg(compression);
    if compression == "xz" {
        cmd.arg("-b").arg("1M")
    } else {
        cmd
    }
}

fn create_mount_point(root: &Path, path: &Path) -> Result<(), String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| format!("{} is not below {} !", path.display(), root.display()))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!(
                    "Refusing to mount on {} because it is a symbolic link!",
                    current.display()
                ));
            }
            Ok(meta) if !meta.is_dir() => {
                return Err(format!("{} is not a directory!", current.display()));
            }
            Ok(_) => {}
            Err(_) => fs::create_dir(&current)
                .map_err(|e| format!("Can not create {}: {e} !", current.display()))?,
        }
    }
    Ok(())
}

/// Removes a directory tree without crossing into mounted file systems.
fn remove_dir(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let canonical =
        fs::canonicalize(path).map_err(|e| format!("Can not resolve {}: {e} !", path.display()))?;
    if canonical == Path::new("/") {
        return Err("Refusing to remove /!".into());
    }
    if let Some(m) = mount_points()
        .into_iter()
        .find(|m| m.starts_with(&canonical))
    {
        return Err(format!(
            "Refusing to remove {}: {} is still mounted!",
            path.display(),
            m.display()
        ));
    }
    fs::remove_dir_all(path).map_err(|e| format!("Can not remove {}: {e} !", path.display()))
}

/// Mount points from `/proc/self/mountinfo`.
pub fn mount_points() -> Vec<PathBuf> {
    fs::read_to_string("/proc/self/mountinfo")
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(|p| PathBuf::from(unescape_mountinfo(p)))
        .collect()
}

fn unescape_mountinfo(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 4 <= bytes.len()
            && bytes[i + 1..i + 4]
                .iter()
                .all(|c| (b'0'..=b'7').contains(c))
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8)
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn is_mount_point(path: &Path) -> bool {
    match fs::canonicalize(path) {
        Ok(p) => mount_points().contains(&p),
        Err(_) => false,
    }
}

/// Searches `PATH` for the given programs and returns the missing ones.
pub fn missing_tools(tools: &[&str]) -> Vec<String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<PathBuf> = std::env::split_paths(&path)
        .chain(["/sbin", "/usr/sbin"].map(PathBuf::from))
        .collect();
    tools
        .iter()
        .filter(|tool| !dirs.iter().any(|d| d.join(tool).is_file()))
        .map(|t| t.to_string())
        .collect()
}

pub fn is_root() -> bool {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(2).map(|uid| uid == "0"))
        })
        .unwrap_or(false)
}

/// Compares version-like strings so that `6.10` sorts after `6.9`.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        match (a.first(), b.first()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, _) => return std::cmp::Ordering::Less,
            (_, None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let na = a.iter().take_while(|c| c.is_ascii_digit()).count();
                let nb = b.iter().take_while(|c| c.is_ascii_digit()).count();
                let da = std::str::from_utf8(&a[..na])
                    .unwrap()
                    .trim_start_matches('0');
                let db = std::str::from_utf8(&b[..nb])
                    .unwrap()
                    .trim_start_matches('0');
                let ord = da.len().cmp(&db.len()).then_with(|| da.cmp(db));
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
                a = &a[na..];
                b = &b[nb..];
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(y);
                }
                a = &a[1..];
                b = &b[1..];
            }
        }
    }
}

/// Finds the newest kernel and its initramfs in `<rootfs>/boot`.
pub fn find_boot_files(rootfs: &Path, initrd: Option<&Path>) -> Result<(PathBuf, PathBuf), String> {
    let boot = rootfs.join("boot");
    let names: Vec<String> = fs::read_dir(&boot)
        .map_err(|e| format!("Can not read {}: {e} !", boot.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    let kernel = names
        .iter()
        .filter(|n| n.starts_with("kernel-") || n.starts_with("vmlinuz-"))
        .max_by(|a, b| natural_cmp(a, b))
        .ok_or_else(|| format!("No kernel found in {} !", boot.display()))?;
    let version = kernel.split_once('-').map(|x| x.1).unwrap_or_default();

    let initramfs = match initrd {
        Some(p) => rootfs.join(p.strip_prefix("/").unwrap_or(p)),
        None => {
            let candidates = [
                format!("initramfs-{version}"),
                format!("initramfs-{version}.img"),
                format!("initrd-{version}"),
                format!("initrd.img-{version}"),
                "initrd".to_string(),
                "initramfs".to_string(),
            ];
            let name = candidates
                .iter()
                .find(|c| names.contains(c))
                .ok_or_else(|| format!("No initramfs for kernel {kernel} found!"))?;
            boot.join(name)
        }
    };
    if !initramfs.is_file() {
        return Err(format!("Initramfs {} not found!", initramfs.display()));
    }
    Ok((boot.join(kernel), initramfs))
}

const SQUASHFS_MAGIC: &[u8; 4] = b"hsqs";

/// Finds the largest SquashFS image in an extracted ISO tree, which is the
/// root file system of a live medium (e.g. `live/pisi.sfs` or
/// `LiveOS/squashfs.img`).
pub fn find_squashfs(dir: &Path) -> Result<PathBuf, String> {
    let mut best: Option<(u64, PathBuf)> = None;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries =
            fs::read_dir(&d).map_err(|e| format!("Can not read {}: {e} !", d.display()))?;
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                let mut magic = [0u8; 4];
                let is_squashfs = fs::File::open(&path)
                    .and_then(|mut f| f.read_exact(&mut magic))
                    .is_ok()
                    && &magic == SQUASHFS_MAGIC;
                if is_squashfs {
                    let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    if best.as_ref().is_none_or(|(s, _)| size > *s) {
                        best = Some((size, path));
                    }
                }
            }
        }
    }
    best.map(|(_, p)| p)
        .ok_or_else(|| format!("No SquashFS root file system found in {} !", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn collecting_executor(dry_run: bool) -> (Executor, Arc<Mutex<Vec<Event>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let e2 = events.clone();
        let sink: EventSink = Arc::new(move |ev| e2.lock().unwrap().push(ev));
        (
            Executor::new(dry_run, Arc::new(AtomicBool::new(false)), sink),
            events,
        )
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("abc/def-1.0"), "abc/def-1.0");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
        let cmd = Cmd::new("echo").arg("hello world").arg("x");
        assert_eq!(cmd.to_string(), "echo 'hello world' x");
    }

    #[test]
    fn natural_sort() {
        use std::cmp::Ordering::*;
        assert_eq!(natural_cmp("kernel-6.10.1", "kernel-6.9.12"), Greater);
        assert_eq!(natural_cmp("kernel-6.1", "kernel-6.1"), Equal);
        assert_eq!(natural_cmp("a", "b"), Less);
    }

    #[test]
    fn mountinfo_unescape() {
        assert_eq!(unescape_mountinfo(r"/mnt/a\040b"), "/mnt/a b");
        assert_eq!(unescape_mountinfo("/plain"), "/plain");
        assert_eq!(unescape_mountinfo(r"/end\"), r"/end\");
    }

    #[test]
    fn finds_boot_files() {
        let dir = tempfile::tempdir().unwrap();
        let boot = dir.path().join("boot");
        fs::create_dir_all(&boot).unwrap();
        for f in [
            "kernel-6.9.1",
            "kernel-6.10.2",
            "initramfs-6.10.2",
            "initramfs-6.9.1",
        ] {
            fs::write(boot.join(f), f).unwrap();
        }
        let (k, i) = find_boot_files(dir.path(), None).unwrap();
        assert!(k.ends_with("kernel-6.10.2"));
        assert!(i.ends_with("initramfs-6.10.2"));
        assert!(find_boot_files(dir.path(), Some(Path::new("/boot/initrd.live"))).is_err());
        fs::write(boot.join("initrd.live"), "x").unwrap();
        let (_, i) = find_boot_files(dir.path(), Some(Path::new("/boot/initrd.live"))).unwrap();
        assert!(i.ends_with("boot/initrd.live"));
    }

    #[test]
    fn finds_largest_squashfs() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("pisi/x86_64");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("pisi.sqfs"), b"hsqs-large-image-data").unwrap();
        fs::write(dir.path().join("small.img"), b"hsqs").unwrap();
        fs::write(
            dir.path().join("other.bin"),
            b"not squashfs at all, but large",
        )
        .unwrap();
        assert_eq!(find_squashfs(dir.path()).unwrap(), sub.join("pisi.sqfs"));
        let empty = tempfile::tempdir().unwrap();
        assert!(find_squashfs(empty.path()).is_err());
    }

    #[test]
    fn dry_run_logs_without_executing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("created");
        let plan = Plan {
            title: "Test".into(),
            required_tools: vec!["definitely-not-installed-tool"],
            steps: vec![Step::new("mkdir", Action::CreateDir(target.clone()))],
            cleanup: vec![],
        };
        let (exec, events) = collecting_executor(true);
        exec.execute(&plan).unwrap();
        assert!(!target.exists());
        let events = events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Log(l) if l.contains("mkdir -p")))
        );
        assert_eq!(events.last(), Some(&Event::Finished(Ok(()))));
    }

    #[test]
    fn runs_commands_and_streams_output() {
        let (exec, events) = collecting_executor(false);
        exec.run_command(&Cmd::new("sh").arg("-c").arg("echo out; echo err >&2"))
            .unwrap();
        assert!(exec.run_command(&Cmd::new("false")).is_err());
        let events = events.lock().unwrap();
        for expected in ["out", "err"] {
            assert!(events.contains(&Event::Log(expected.into())));
        }
    }

    #[test]
    fn missing_tools_detection() {
        assert_eq!(missing_tools(&["sh"]), Vec::<String>::new());
        assert_eq!(
            missing_tools(&["no-such-tool-xyz"]),
            vec!["no-such-tool-xyz"]
        );
    }

    #[test]
    fn mount_points_reject_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        create_mount_point(root, &root.join("dev/pts")).unwrap();
        assert!(root.join("dev/pts").is_dir());
        create_mount_point(root, &root.join("dev/pts")).unwrap();
        std::os::unix::fs::symlink("/etc", root.join("proc")).unwrap();
        assert!(create_mount_point(root, &root.join("proc")).is_err());
        std::os::unix::fs::symlink("/", root.join("run")).unwrap();
        assert!(create_mount_point(root, &root.join("run/x")).is_err());
        assert!(create_mount_point(root, Path::new("/elsewhere")).is_err());
    }

    #[test]
    fn remove_dir_works() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("a/b");
        fs::create_dir_all(&sub).unwrap();
        remove_dir(&dir.path().join("a")).unwrap();
        assert!(!dir.path().join("a").exists());
        remove_dir(&dir.path().join("missing")).unwrap();
    }
}
