//! ISO project and ISO edit job definitions.
//!
//! Both are stored as TOML files so they can be versioned, shared and used
//! from the command line as well as from the graphical interface.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// SquashFS compressors supported by `mksquashfs`.
pub const COMPRESSIONS: &[&str] = &["xz", "zstd", "gzip", "lzo", "lz4"];

/// A PiSi package repository (`pisi add-repo <name> <url>`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Repository {
    pub name: String,
    pub url: String,
}

impl Repository {
    pub fn new(name: &str, url: &str) -> Self {
        Self {
            name: name.to_string(),
            url: url.to_string(),
        }
    }
}

/// Description of a live ISO image that is built from PiSi packages.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct Project {
    /// Human readable distribution name, used in the boot menu.
    pub name: String,
    /// Distribution / release version, used in the boot menu.
    pub version: String,
    /// ISO9660 volume label. Also used by the initramfs to find the medium.
    pub volume_label: String,
    /// Hostname of the live system.
    pub hostname: String,
    /// Passwordless live user. Leave empty to not create one.
    pub live_user: String,
    /// PiSi repositories used to install the root file system.
    pub repositories: Vec<Repository>,
    /// PiSi components installed with `pisi install -c`.
    pub components: Vec<String>,
    /// PiSi packages installed with `pisi install`.
    pub packages: Vec<String>,
    /// PiSi packages removed again after installation.
    pub excluded_packages: Vec<String>,
    /// Directory used for the root file system and ISO tree.
    pub work_dir: PathBuf,
    /// Path of the resulting ISO image.
    pub output_iso: PathBuf,
    /// SquashFS compression algorithm.
    pub squashfs_compression: String,
    /// Extra kernel command line options for the live system.
    pub kernel_cmdline: String,
    /// Generate a live capable initramfs with mkinitcpio and Pisi's `live`
    /// hook (kernel parameter `boot=live`, image at `/live/pisi.sfs`).
    pub live_initramfs: bool,
    /// Shell script executed inside the chroot after the packages are configured.
    pub post_install_script: String,
}

impl Default for Project {
    fn default() -> Self {
        Self {
            name: "Pisi GNU/Linux".into(),
            version: "2.0".into(),
            volume_label: "PISI_LIVE".into(),
            hostname: "pisi".into(),
            live_user: "pisi".into(),
            repositories: vec![Repository::new(
                "pisi-2.0",
                "https://ciftlik.pisilinux.org/pisi-2.0/pisi-index.xml.xz",
            )],
            components: vec!["system.base".into()],
            packages: vec!["kernel".into(), "mkinitcpio".into(), "grub2".into()],
            excluded_packages: vec![],
            work_dir: PathBuf::from("/var/tmp/pisi-iso-creator"),
            output_iso: PathBuf::from("pisi-live.iso"),
            squashfs_compression: "xz".into(),
            kernel_cmdline: "quiet splash".into(),
            live_initramfs: true,
            post_install_script: String::new(),
        }
    }
}

impl Project {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text =
            fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        toml::from_str(&text).map_err(|e| format!("invalid project file {}: {e}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    /// Returns a list of human readable problems. An empty list means the
    /// project can be built.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if self.name.trim().is_empty() {
            errors.push("Distribution name must not be empty.".into());
        }
        if has_control_chars(&self.name) || has_control_chars(&self.version) {
            errors.push("Name and version must not contain control characters.".into());
        }
        check_volume_label(&self.volume_label, &mut errors);
        if !self.hostname.is_empty() && !is_valid_hostname(&self.hostname) {
            errors.push(format!("Invalid hostname '{}'.", self.hostname));
        }
        if !self.live_user.is_empty() && !is_valid_user_name(&self.live_user) {
            errors.push(format!("Invalid live user name '{}'.", self.live_user));
        }
        if self.repositories.is_empty() {
            errors.push("At least one PiSi repository is required.".into());
        }
        check_repositories(&self.repositories, &mut errors);
        if self.components.is_empty() && self.packages.is_empty() {
            errors.push("Select at least one PiSi component or package.".into());
        }
        check_names("component", &self.components, &mut errors);
        check_names("package", &self.packages, &mut errors);
        check_names("excluded package", &self.excluded_packages, &mut errors);
        check_paths(&self.work_dir, &self.output_iso, &mut errors);
        check_compression(&self.squashfs_compression, &mut errors);
        if has_control_chars(&self.kernel_cmdline) {
            errors.push("Kernel command line must be a single line.".into());
        }
        errors
    }
}

/// Description of a modification of an existing (PiSi based) live ISO image.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct EditJob {
    pub input_iso: PathBuf,
    pub output_iso: PathBuf,
    pub work_dir: PathBuf,
    /// New volume label; empty keeps the original one.
    pub volume_label: String,
    /// Repositories added to the image before packages are changed.
    pub repositories: Vec<Repository>,
    /// Run `pisi upgrade` on the image.
    pub upgrade: bool,
    pub install_packages: Vec<String>,
    pub remove_packages: Vec<String>,
    pub squashfs_compression: String,
    pub post_install_script: String,
}

impl Default for EditJob {
    fn default() -> Self {
        Self {
            input_iso: PathBuf::new(),
            output_iso: PathBuf::from("pisi-edited.iso"),
            work_dir: PathBuf::from("/var/tmp/pisi-iso-editor"),
            volume_label: String::new(),
            repositories: vec![],
            upgrade: false,
            install_packages: vec![],
            remove_packages: vec![],
            squashfs_compression: "xz".into(),
            post_install_script: String::new(),
        }
    }
}

impl EditJob {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text =
            fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        toml::from_str(&text).map_err(|e| format!("invalid edit job file {}: {e}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if self.input_iso.as_os_str().is_empty() {
            errors.push("Select the ISO image to edit.".into());
        }
        if !self.volume_label.is_empty() {
            check_volume_label(&self.volume_label, &mut errors);
        }
        check_repositories(&self.repositories, &mut errors);
        check_names("package", &self.install_packages, &mut errors);
        check_names("package", &self.remove_packages, &mut errors);
        check_paths(&self.work_dir, &self.output_iso, &mut errors);
        if !self.input_iso.as_os_str().is_empty() && self.input_iso == self.output_iso {
            errors.push("Output ISO must be different from the input ISO.".into());
        }
        check_compression(&self.squashfs_compression, &mut errors);
        errors
    }
}

fn has_control_chars(s: &str) -> bool {
    s.chars().any(char::is_control)
}

fn check_volume_label(label: &str, errors: &mut Vec<String>) {
    if label.is_empty() || label.len() > 32 {
        errors.push("Volume label must be 1 to 32 characters long.".into());
    }
    if !label
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        errors.push("Volume label may only contain A-Z, a-z, 0-9, '_' and '-'.".into());
    }
}

fn check_repositories(repos: &[Repository], errors: &mut Vec<String>) {
    for repo in repos {
        if !is_valid_name(&repo.name) {
            errors.push(format!("Invalid repository name '{}'.", repo.name));
        }
        if !is_valid_url(&repo.url) {
            errors.push(format!("Invalid repository URL '{}'.", repo.url));
        }
    }
}

fn check_names(kind: &str, names: &[String], errors: &mut Vec<String>) {
    for name in names {
        if !is_valid_name(name) {
            errors.push(format!("Invalid {kind} name '{name}'."));
        }
    }
}

fn check_paths(work_dir: &Path, output_iso: &Path, errors: &mut Vec<String>) {
    if work_dir.as_os_str().is_empty() {
        errors.push("Work directory must not be empty.".into());
    } else if work_dir == Path::new("/") {
        errors.push("Work directory must not be the root directory.".into());
    }
    if output_iso.as_os_str().is_empty() {
        errors.push("Output ISO path must not be empty.".into());
    }
}

fn check_compression(compression: &str, errors: &mut Vec<String>) {
    if !COMPRESSIONS.contains(&compression) {
        errors.push(format!("Unsupported SquashFS compression '{compression}'."));
    }
}

/// PiSi package, component and repository names. Names must not start with
/// '-' so they can never be interpreted as command line options.
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
}

/// Repository index location: an http(s)/ftp URL or an absolute local path.
pub fn is_valid_url(url: &str) -> bool {
    let ok_scheme = ["http://", "https://", "ftp://", "file://", "/"]
        .iter()
        .any(|p| url.starts_with(p));
    ok_scheme && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

pub fn is_valid_hostname(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

pub fn is_valid_user_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c == '_' => {}
        _ => return false,
    }
    name.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Parses a whitespace / newline separated list, ignoring `#` comments.
pub fn parse_list(text: &str) -> Vec<String> {
    let mut items: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default();
        for item in line.split([' ', '\t', ',']) {
            let item = item.trim();
            if !item.is_empty() && !items.iter().any(|i| i == item) {
                items.push(item.to_string());
            }
        }
    }
    items
}

pub fn format_list(items: &[String]) -> String {
    items.join("\n")
}

/// Parses `name url` lines into repositories, ignoring `#` comments.
pub fn parse_repositories(text: &str) -> Result<Vec<Repository>, String> {
    let mut repos = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next(), parts.next()) {
            (Some(name), Some(url), None) => repos.push(Repository::new(name, url)),
            _ => {
                return Err(format!(
                    "repository line {} must have the form '<name> <index url>'",
                    n + 1
                ));
            }
        }
    }
    Ok(repos)
}

pub fn format_repositories(repos: &[Repository]) -> String {
    repos
        .iter()
        .map(|r| format!("{} {}", r.name, r.url))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_project_is_valid() {
        assert_eq!(Project::default().validate(), Vec::<String>::new());
    }

    #[test]
    fn rejects_option_like_names() {
        let mut p = Project::default();
        p.packages.push("--force".into());
        p.volume_label = "bad label".into();
        let errors = p.validate();
        assert!(errors.iter().any(|e| e.contains("--force")));
        assert!(errors.iter().any(|e| e.contains("Volume label")));
    }

    #[test]
    fn project_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.toml");
        let p = Project {
            excluded_packages: vec!["nano".into()],
            ..Default::default()
        };
        p.save(&path).unwrap();
        assert_eq!(Project::load(&path).unwrap(), p);
    }

    #[test]
    fn edit_job_round_trip_and_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e.toml");
        let mut job = EditJob::default();
        assert!(!job.validate().is_empty());
        job.input_iso = "in.iso".into();
        job.install_packages = vec!["firefox".into()];
        assert!(job.validate().is_empty());
        job.save(&path).unwrap();
        assert_eq!(EditJob::load(&path).unwrap(), job);
        job.output_iso = "in.iso".into();
        assert!(!job.validate().is_empty());
    }

    #[test]
    fn list_parsing() {
        assert_eq!(
            parse_list("a b\n# comment\nc, d # trailing\n a"),
            vec!["a", "b", "c", "d"]
        );
        let repos = parse_repositories("# x\npisi https://x/pisi-index.xml.xz\n").unwrap();
        assert_eq!(
            repos,
            vec![Repository::new("pisi", "https://x/pisi-index.xml.xz")]
        );
        assert!(parse_repositories("only-name").is_err());
        assert_eq!(
            format_repositories(&repos),
            "pisi https://x/pisi-index.xml.xz"
        );
    }

    #[test]
    fn name_validation() {
        assert!(is_valid_name("gtk+3"));
        assert!(is_valid_name("system.base"));
        assert!(!is_valid_name("-y"));
        assert!(!is_valid_name("a b"));
        assert!(is_valid_user_name("pisi"));
        assert!(!is_valid_user_name("Root"));
        assert!(is_valid_hostname("pisi-live"));
        assert!(!is_valid_hostname("pisi_live"));
        assert!(is_valid_url("https://x/y.xml.xz"));
        assert!(!is_valid_url("-o/x"));
    }
}
