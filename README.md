# Pisi ISO Creator

Create and edit live ISO images of **Pisi GNU/Linux** from **PiSi packages**.
Written in Rust with a [Slint](https://slint.dev) user interface; every
feature is also available from the command line.

## Features

- **ISO creator** – builds a bootable hybrid (BIOS + UEFI) live ISO:
  1. installs PiSi components and packages from the configured PiSi
     repositories into a fresh root file system (`pisi --destdir=…`),
  2. runs the COMAR scripts inside a chroot (`pisi configure-pending`),
  3. sets the hostname, creates a passwordless live user and runs an optional
     post-install script,
  4. generates a live initramfs with `mkinitcpio` and the `live` hook shipped
     by Pisi's mkinitcpio package (`boot=live`),
  5. compresses the root file system to `live/pisi.sfs` with SquashFS,
  6. writes a GRUB menu and creates the ISO with `grub-mkrescue`.
- **ISO editor** – modifies an existing (Pisi based) live ISO: extracts it,
  unpacks the root SquashFS (found automatically), adds repositories,
  upgrades / installs / removes PiSi packages, runs a post-install script,
  recompresses the image and writes a new ISO with the original boot setup
  (`xorriso -boot_image any replay`), optionally with a new volume label.
- **Repository browser** – loads a `pisi-index.xml` or `pisi-index.xml.xz`
  (URL or local file, binary and source indexes), searches packages and adds
  them to the project with a click.
- **Dry run and plan view** – shows every command before anything is executed.
- Live, streaming build log, progress and cancellation; mounts are always
  cleaned up, even after failures.
- Projects and edit jobs are plain TOML files (see [`examples/`](examples)).

## Requirements

Building the application (Debian/Ubuntu package names):

```sh
sudo apt install libfontconfig1-dev libxkbcommon-dev   # plus a Rust toolchain
```

On Pisi GNU/Linux install `fontconfig-devel` and `libxkbcommon-devel`.

Creating and editing images requires **root** and these host tools:

| Task        | Tools                                                                 |
|-------------|-----------------------------------------------------------------------|
| Create ISO  | `pisi`, `chroot`, `mount`, `mksquashfs`, `grub-mkrescue`, `xorriso`, `mformat` (mtools) |
| Edit ISO    | `xorriso`, `unsquashfs`, `mksquashfs`; `pisi`, `chroot`, `mount` when packages are changed |

The host is best a Pisi GNU/Linux system, so that the host `pisi` matches the
PiSi database format of the image. Missing tools are reported before a run
starts.

## Build and run

```sh
cargo build --release
sudo ./target/release/pisi-iso-creator          # graphical interface
```

Command line usage:

```text
pisi-iso-creator new <project.toml>                       write a default project
pisi-iso-creator build <project.toml> [--dry-run|--plan]  build an ISO
pisi-iso-creator edit <edit-job.toml> [--dry-run|--plan]  edit an existing ISO
pisi-iso-creator search <index URL or path> [query]       search a PiSi repository
```

`--plan` prints the complete pipeline as a shell-like script, `--dry-run`
walks through all steps without executing anything (no root required).

## Project file

```toml
name = "Pisi GNU/Linux"
version = "2.0"
volume_label = "PISI_LIVE"          # ISO9660 label, max. 32 chars [A-Za-z0-9_-]
hostname = "pisi"
live_user = "pisi"                  # empty = no live user
components = ["system.base"]        # pisi install --component …
packages = ["kernel", "mkinitcpio", "grub2"]
excluded_packages = []              # removed again after installation
work_dir = "/var/tmp/pisi-iso-creator"
output_iso = "pisi-live.iso"
squashfs_compression = "xz"         # xz, zstd, gzip, lzo, lz4
kernel_cmdline = "quiet splash"
live_initramfs = true               # mkinitcpio + Pisi "live" hook
post_install_script = ""            # executed with /bin/sh -e in the chroot

[[repositories]]
name = "pisi-2.0"
url = "https://ciftlik.pisilinux.org/pisi-2.0/pisi-index.xml.xz"
```

Adjust the repository URL to the binary repository you want to build from.
Inside the work directory the tool uses `rootfs/` and `iso/`; both are
recreated on every run.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Source layout:

- `src/project.rs` – project / edit job model, validation, TOML I/O
- `src/pisi.rs` – PiSi command lines and repository index parser
- `src/pipeline.rs` – creator and editor build plans, GRUB configuration
- `src/command.rs` – step executor (logging, dry run, cancellation, cleanup)
- `src/gui.rs`, `ui/app.slint` – Slint user interface
- `src/main.rs` – command line entry point
