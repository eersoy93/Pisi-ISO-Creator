//! Build plans for creating a new live ISO from PiSi packages and for
//! editing an existing ISO image.

use crate::command::{Action, Cmd, Plan, Step, mksquashfs};
use crate::pisi::{Pisi, chroot};
use crate::project::{EditJob, Project};
use std::path::{Path, PathBuf};

const POST_INSTALL_SCRIPT: &str = "tmp/pisi-iso-post-install.sh";
const LIVE_INITRD: &str = "/boot/initrd.live";
const LIVE_MKINITCPIO_CONF: &str = "etc/mkinitcpio-live.conf";
/// Location of the root file system image expected by the `live` hook of
/// Pisi's mkinitcpio package (`/usr/lib/initcpio/hooks/live`).
pub const LIVE_SQUASHFS: &str = "live/pisi.sfs";

/// Plan for building `project.output_iso` from scratch.
pub fn build_plan(project: &Project) -> Plan {
    let work = &project.work_dir;
    let rootfs = work.join("rootfs");
    let iso = work.join("iso");
    let pisi = Pisi::new(&rootfs);
    let mut steps = vec![
        Step::new(
            "Remove previous root file system",
            Action::RemoveDir(rootfs.clone()),
        ),
        Step::new("Remove previous ISO tree", Action::RemoveDir(iso.clone())),
        Step::new(
            "Create root file system directory",
            Action::CreateDir(rootfs.clone()),
        ),
        Step::new("Create ISO tree", Action::CreateDir(iso.join("live"))),
    ];
    for repo in &project.repositories {
        steps.push(Step::run(
            format!("Add PiSi repository '{}'", repo.name),
            pisi.add_repo(&repo.name, &repo.url),
        ));
    }
    if !project.components.is_empty() {
        steps.push(Step::run(
            format!("Install components: {}", project.components.join(", ")),
            pisi.install_components(&project.components),
        ));
    }
    if !project.packages.is_empty() {
        steps.push(Step::run(
            format!("Install {} package(s)", project.packages.len()),
            pisi.install(&project.packages),
        ));
    }
    if !project.excluded_packages.is_empty() {
        steps.push(Step::run(
            format!(
                "Remove excluded package(s): {}",
                project.excluded_packages.join(", ")
            ),
            pisi.remove(&project.excluded_packages),
        ));
    }

    steps.extend(enter_chroot_steps(&rootfs, true));
    steps.push(Step::run(
        "Configure baselayout",
        pisi.configure_pending_in_chroot(Some("baselayout")),
    ));
    steps.push(Step::run(
        "Configure pending packages (COMAR)",
        pisi.configure_pending_in_chroot(None),
    ));
    if !project.hostname.is_empty() {
        steps.push(Step::new(
            "Set hostname",
            Action::WriteFile {
                path: rootfs.join("etc/hostname"),
                contents: format!("{}\n", project.hostname),
            },
        ));
    }
    if !project.live_user.is_empty() {
        steps.push(Step::run(
            format!("Create live user '{}'", project.live_user),
            chroot(&rootfs, "useradd")
                .args(["-m", "-s", "/bin/bash", "-G", "wheel"])
                .arg(&project.live_user),
        ));
        steps.push(Step::run(
            "Allow passwordless live login",
            chroot(&rootfs, "passwd").arg("-d").arg(&project.live_user),
        ));
    }
    steps.extend(post_install_steps(&rootfs, &project.post_install_script));
    if project.live_initramfs {
        steps.push(Step::new(
            "Write live mkinitcpio configuration",
            Action::WriteFile {
                path: rootfs.join(LIVE_MKINITCPIO_CONF),
                contents: LIVE_MKINITCPIO_CONFIG.into(),
            },
        ));
        steps.push(Step::run(
            "Generate live initramfs with mkinitcpio",
            chroot(&rootfs, "/bin/sh").arg("-c").arg(format!(
                "kver=$(ls /lib/modules | sort -V | tail -n 1) && \
                 mkinitcpio -k \"$kver\" -c /{LIVE_MKINITCPIO_CONF} -g {LIVE_INITRD}"
            )),
        ));
    }
    steps.push(Step::run(
        "Delete PiSi package cache",
        pisi.delete_cache_in_chroot(),
    ));
    steps.extend(leave_chroot_steps(&rootfs, true));

    steps.push(Step::new(
        "Copy kernel and initramfs",
        Action::CopyBootFiles {
            rootfs: rootfs.clone(),
            dest: iso.join("boot"),
            initrd: project.live_initramfs.then(|| PathBuf::from(LIVE_INITRD)),
        },
    ));
    steps.push(Step::run(
        "Compress root file system (SquashFS)",
        mksquashfs(
            &rootfs,
            &iso.join(LIVE_SQUASHFS),
            &project.squashfs_compression,
        ),
    ));
    steps.push(Step::new(
        "Write GRUB configuration",
        Action::WriteFile {
            path: iso.join("boot/grub/grub.cfg"),
            contents: grub_config(project),
        },
    ));
    if let Some(parent) = non_empty_parent(&project.output_iso) {
        steps.push(Step::new(
            "Create output directory",
            Action::CreateDir(parent),
        ));
    }
    steps.push(Step::run(
        "Create bootable hybrid ISO image (BIOS + UEFI)",
        Cmd::new("grub-mkrescue")
            .arg("-o")
            .arg(&project.output_iso)
            .arg(&iso)
            .arg("--")
            .arg("-volid")
            .arg(&project.volume_label),
    ));

    Plan {
        title: format!("Build {} {} live ISO", project.name, project.version),
        required_tools: vec![
            "pisi",
            "chroot",
            "mount",
            "umount",
            "mksquashfs",
            "grub-mkrescue",
            "xorriso",
            "mformat",
        ],
        steps,
        cleanup: cleanup_steps(&rootfs),
    }
}

/// Plan for modifying an existing ISO image.
pub fn edit_plan(job: &EditJob) -> Plan {
    let work = &job.work_dir;
    let rootfs = work.join("rootfs");
    let iso = work.join("iso");
    let pisi = Pisi::new(&rootfs);
    let mut steps = vec![
        Step::new(
            "Remove previous root file system",
            Action::RemoveDir(rootfs.clone()),
        ),
        Step::new("Remove previous ISO tree", Action::RemoveDir(iso.clone())),
        Step::new("Create work directory", Action::CreateDir(work.clone())),
        Step::run(
            "Extract ISO image",
            Cmd::new("xorriso")
                .args(["-osirrox", "on", "-indev"])
                .arg(&job.input_iso)
                .args(["-extract", "/"])
                .arg(&iso),
        ),
        Step::new(
            "Unpack root file system",
            Action::UnpackSquashfs {
                iso_dir: iso.clone(),
                rootfs: rootfs.clone(),
            },
        ),
    ];
    for repo in &job.repositories {
        steps.push(Step::run(
            format!("Add PiSi repository '{}'", repo.name),
            pisi.add_repo(&repo.name, &repo.url),
        ));
    }
    let changes_packages =
        job.upgrade || !job.install_packages.is_empty() || !job.remove_packages.is_empty();
    if job.upgrade || !job.install_packages.is_empty() {
        steps.push(Step::run("Update PiSi repositories", pisi.update_repo()));
    }
    if job.upgrade {
        steps.push(Step::run("Upgrade installed packages", pisi.upgrade()));
    }
    if !job.remove_packages.is_empty() {
        steps.push(Step::run(
            format!("Remove package(s): {}", job.remove_packages.join(", ")),
            pisi.remove(&job.remove_packages),
        ));
    }
    if !job.install_packages.is_empty() {
        steps.push(Step::run(
            format!("Install package(s): {}", job.install_packages.join(", ")),
            pisi.install(&job.install_packages),
        ));
    }
    if changes_packages || !job.post_install_script.trim().is_empty() {
        steps.extend(enter_chroot_steps(&rootfs, changes_packages));
        if changes_packages {
            steps.push(Step::run(
                "Configure pending packages (COMAR)",
                pisi.configure_pending_in_chroot(None),
            ));
        }
        steps.extend(post_install_steps(&rootfs, &job.post_install_script));
        if changes_packages {
            steps.push(Step::run(
                "Delete PiSi package cache",
                pisi.delete_cache_in_chroot(),
            ));
        }
        steps.extend(leave_chroot_steps(&rootfs, changes_packages));
    }
    steps.push(Step::new(
        "Recompress root file system (SquashFS)",
        Action::RepackSquashfs {
            rootfs: rootfs.clone(),
            compression: job.squashfs_compression.clone(),
        },
    ));
    if let Some(parent) = non_empty_parent(&job.output_iso) {
        steps.push(Step::new(
            "Create output directory",
            Action::CreateDir(parent),
        ));
    }
    steps.push(Step::run(
        "Remove old output ISO",
        Cmd::new("rm").arg("-f").arg(&job.output_iso),
    ));
    let mut xorriso = Cmd::new("xorriso")
        .arg("-indev")
        .arg(&job.input_iso)
        .arg("-outdev")
        .arg(&job.output_iso)
        .args(["-boot_image", "any", "replay"]);
    if !job.volume_label.is_empty() {
        xorriso = xorriso.arg("-volid").arg(&job.volume_label);
    }
    steps.push(Step::run(
        "Write modified ISO image (keeping original boot setup)",
        xorriso.arg("-map").arg(&iso).arg("/"),
    ));

    let mut required_tools = vec!["xorriso", "unsquashfs", "mksquashfs"];
    if changes_packages || !job.repositories.is_empty() {
        required_tools.push("pisi");
    }
    if changes_packages || !job.post_install_script.trim().is_empty() {
        required_tools.extend(["chroot", "mount", "umount"]);
    }
    Plan {
        title: format!("Edit ISO image {}", job.input_iso.display()),
        required_tools,
        steps,
        cleanup: cleanup_steps(&rootfs),
    }
}

fn non_empty_parent(path: &Path) -> Option<PathBuf> {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
}

/// API file systems needed inside the chroot, in mount order.
fn api_mounts(rootfs: &Path) -> Vec<(PathBuf, Cmd)> {
    let m = |sub: &str| rootfs.join(sub);
    vec![
        (
            m("proc"),
            Cmd::new("mount")
                .args(["-t", "proc", "proc"])
                .arg(m("proc")),
        ),
        (
            m("sys"),
            Cmd::new("mount")
                .args(["-t", "sysfs", "sysfs"])
                .arg(m("sys")),
        ),
        (
            m("dev"),
            Cmd::new("mount").args(["--bind", "/dev"]).arg(m("dev")),
        ),
        (
            m("dev/pts"),
            Cmd::new("mount")
                .args(["--bind", "/dev/pts"])
                .arg(m("dev/pts")),
        ),
        (
            m("run"),
            Cmd::new("mount")
                .args(["-t", "tmpfs", "tmpfs"])
                .arg(m("run")),
        ),
    ]
}

/// Mounts the API file systems and, if `with_dbus` is set, starts the D-Bus
/// system bus that COMAR (`pisi configure-pending`) needs.
fn enter_chroot_steps(rootfs: &Path, with_dbus: bool) -> Vec<Step> {
    let mut steps = Vec::new();
    for (dir, cmd) in api_mounts(rootfs) {
        steps.push(Step::new(
            format!("Create {}", dir.display()),
            Action::CreateDir(dir.clone()),
        ));
        steps.push(Step::run(format!("Mount {}", dir.display()), cmd));
    }
    if !with_dbus {
        return steps;
    }
    steps.push(Step::new(
        "Prepare D-Bus runtime directory",
        Action::CreateDir(rootfs.join("run/dbus")),
    ));
    steps.push(Step::run(
        "Generate D-Bus machine id",
        chroot(rootfs, "/usr/bin/dbus-uuidgen").arg("--ensure"),
    ));
    steps.push(Step::run(
        "Start D-Bus (required by COMAR)",
        chroot(rootfs, "/sbin/start-stop-daemon").args([
            "-b",
            "--start",
            "--pidfile",
            "/run/dbus/pid",
            "--exec",
            "/usr/bin/dbus-daemon",
            "--",
            "--system",
        ]),
    ));
    steps.push(Step::new(
        "Wait for D-Bus",
        Action::WaitForFile {
            path: rootfs.join("run/dbus/system_bus_socket"),
            timeout_secs: 30,
        },
    ));
    steps
}

fn stop_dbus(rootfs: &Path) -> Step {
    Step::run(
        "Stop D-Bus",
        chroot(rootfs, "/sbin/start-stop-daemon")
            .args(["--stop", "--pidfile", "/run/dbus/pid"])
            .allow_failure(),
    )
}

fn unmount_steps(rootfs: &Path) -> Vec<Step> {
    api_mounts(rootfs)
        .into_iter()
        .rev()
        .map(|(dir, _)| Step::new(format!("Unmount {}", dir.display()), Action::Unmount(dir)))
        .collect()
}

fn leave_chroot_steps(rootfs: &Path, with_dbus: bool) -> Vec<Step> {
    let mut steps = if with_dbus {
        vec![stop_dbus(rootfs)]
    } else {
        vec![]
    };
    steps.extend(unmount_steps(rootfs));
    steps
}

/// Cleanup is idempotent: unmounting is skipped for paths that are not
/// mounted, so it is safe after both successful and failed runs.
fn cleanup_steps(rootfs: &Path) -> Vec<Step> {
    unmount_steps(rootfs)
}

fn post_install_steps(rootfs: &Path, script: &str) -> Vec<Step> {
    if script.trim().is_empty() {
        return vec![];
    }
    let path = rootfs.join(POST_INSTALL_SCRIPT);
    vec![
        Step::new(
            "Write post-install script",
            Action::WriteFile {
                path: path.clone(),
                contents: format!("{}\n", script.trim_end()),
            },
        ),
        Step::run(
            "Run post-install script in chroot",
            chroot(rootfs, "/bin/sh")
                .arg("-e")
                .arg(format!("/{POST_INSTALL_SCRIPT}")),
        ),
        Step::run(
            "Remove post-install script",
            Cmd::new("rm").arg("-f").arg(path),
        ),
    ]
}

/// mkinitcpio configuration for a generic (not host specific) live initramfs
/// using the `live` hook shipped by Pisi's mkinitcpio package.
const LIVE_MKINITCPIO_CONFIG: &str = "\
# Generated by Pisi ISO Creator
MODULES=(loop squashfs overlay isofs sr_mod cdrom)
BINARIES=()
FILES=()
HOOKS=(base udev modconf block filesystems keyboard live)
COMPRESSION=\"gzip\"
";

/// Escapes a string for use inside single quotes in grub.cfg.
fn grub_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

pub fn grub_config(project: &Project) -> String {
    let title = format!("{} {}", project.name, project.version);
    let mut cmdline = String::new();
    if project.live_initramfs {
        cmdline.push_str("boot=live");
    }
    if !project.kernel_cmdline.trim().is_empty() {
        if !cmdline.is_empty() {
            cmdline.push(' ');
        }
        cmdline.push_str(project.kernel_cmdline.trim());
    }
    let entry = |name: String, extra: &str| {
        format!(
            "menuentry {} {{\n    linux /boot/kernel {cmdline}{extra}\n    initrd /boot/initrd\n}}\n\n",
            grub_quote(&name)
        )
    };
    let mut cfg = format!(
        "# Generated by Pisi ISO Creator\n\
         set default=0\n\
         set timeout=10\n\
         insmod all_video\n\
         insmod gfxterm\n\
         search --no-floppy --set=root --file /{LIVE_SQUASHFS}\n\n"
    );
    cfg.push_str(&entry(format!("{title} (Live)"), ""));
    cfg.push_str(&entry(
        format!("{title} (Live, safe graphics)"),
        " nomodeset",
    ));
    cfg.push_str("menuentry 'Reboot' {\n    reboot\n}\n\nmenuentry 'Power off' {\n    halt\n}\n");
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Repository;

    fn commands(plan: &Plan) -> Vec<String> {
        plan.steps
            .iter()
            .filter_map(|s| match &s.action {
                Action::Run(c) => Some(c.to_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn build_plan_contains_pipeline() {
        let project = Project {
            work_dir: "/w".into(),
            output_iso: "/out/live.iso".into(),
            post_install_script: "echo hi".into(),
            ..Default::default()
        };
        let plan = build_plan(&project);
        let cmds = commands(&plan);
        let pos = |needle: &str| {
            cmds.iter()
                .position(|c| c.contains(needle))
                .unwrap_or_else(|| panic!("missing {needle} in {cmds:#?}"))
        };
        let order = [
            "add-repo pisi",
            "install --ignore-comar --ignore-file-conflicts --component system.base",
            "install --ignore-comar --ignore-file-conflicts kernel",
            "mount -t proc proc /w/rootfs/proc",
            "configure-pending baselayout",
            "useradd -m",
            "/bin/sh -e /tmp/pisi-iso-post-install.sh",
            "mkinitcpio -k",
            "delete-cache",
            "mksquashfs /w/rootfs /w/iso/live/pisi.sfs -noappend -comp xz",
            "grub-mkrescue -o /out/live.iso /w/iso -- -volid PISI_LIVE",
        ];
        for pair in order.windows(2) {
            assert!(
                pos(pair[0]) < pos(pair[1]),
                "{} before {}",
                pair[0],
                pair[1]
            );
        }
        assert!(plan.steps.iter().any(|s| matches!(
            &s.action,
            Action::CopyBootFiles { initrd: Some(p), .. } if p == Path::new(LIVE_INITRD)
        )));
        assert_eq!(plan.cleanup.len(), 5);
        assert!(plan.describe().contains("grub.cfg"));
    }

    #[test]
    fn build_plan_skips_optional_steps() {
        let mut project = Project::default();
        project.live_user.clear();
        project.excluded_packages.clear();
        project.live_initramfs = false;
        let cmds = commands(&build_plan(&project));
        assert!(
            !cmds
                .iter()
                .any(|c| c.contains("useradd") || c.contains("mkinitcpio -k"))
        );
        assert!(!cmds.iter().any(|c| c.contains(" remove ")));
        assert!(!cmds.iter().any(|c| c.contains("post-install")));
    }

    #[test]
    fn grub_config_is_correct() {
        let mut project = Project {
            name: "Pisi's Linux".into(),
            ..Default::default()
        };
        let cfg = grub_config(&project);
        assert!(cfg.contains("menuentry 'Pisi'\\''s Linux 2.0 (Live)'"));
        assert!(cfg.contains("linux /boot/kernel boot=live quiet splash\n"));
        assert!(cfg.contains("nomodeset"));
        assert!(cfg.contains("--file /live/pisi.sfs"));
        project.live_initramfs = false;
        assert!(!grub_config(&project).contains("boot=live"));
    }

    #[test]
    fn edit_plan_contains_pipeline() {
        let job = EditJob {
            input_iso: "/in.iso".into(),
            output_iso: "out.iso".into(),
            work_dir: "/w".into(),
            volume_label: "NEW".into(),
            repositories: vec![Repository::new("extra", "https://x/pisi-index.xml.xz")],
            upgrade: true,
            install_packages: vec!["firefox".into()],
            remove_packages: vec!["nano".into()],
            squashfs_compression: "zstd".into(),
            post_install_script: String::new(),
        };
        let plan = edit_plan(&job);
        let cmds = commands(&plan);
        assert_eq!(
            cmds[0],
            "xorriso -osirrox on -indev /in.iso -extract / /w/iso"
        );
        let joined = cmds.join("\n");
        for needle in [
            "add-repo extra",
            "update-repo",
            "upgrade",
            "remove --ignore-comar nano",
            "firefox",
            "configure-pending",
        ] {
            assert!(joined.contains(needle), "missing {needle}");
        }
        assert_eq!(
            cmds.last().unwrap(),
            "xorriso -indev /in.iso -outdev out.iso -boot_image any replay -volid NEW -map /w/iso /"
        );
        assert!(plan.required_tools.contains(&"pisi"));
        assert!(plan.steps.iter().any(|s| matches!(
            &s.action,
            Action::RepackSquashfs { compression, .. } if compression == "zstd"
        )));
    }

    #[test]
    fn edit_plan_without_changes_skips_chroot() {
        let job = EditJob {
            input_iso: "/in.iso".into(),
            ..Default::default()
        };
        let plan = edit_plan(&job);
        let cmds = commands(&plan);
        assert!(
            !cmds
                .iter()
                .any(|c| c.starts_with("chroot ") || c.starts_with("pisi "))
        );
        let joined = cmds.join("\n");
        assert!(!plan.required_tools.contains(&"pisi"));
        assert!(!joined.contains("-volid"));
    }
}
