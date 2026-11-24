//! Conversion of Pisiman project files (`<PisiManProject>` XML) into
//! Pisi ISO Creator projects.
//!
//! Pisiman describes an image with a title, a work directory, one
//! repository (`repo_uri`) and package selections (`SelectedComponent`,
//! `SelectedPackage` and the fully resolved `Package` list), optionally split
//! into root / desktop / live CD images or package collections. All selected
//! components and packages are merged into a single live image; settings
//! without an equivalent are reported as warnings.
//!
//! - `type` (`Sddm`, `Lightdm`, `Lxdm`, `Mdm`) becomes `display_manager`,
//! - `PluginPackage` is added to the packages,
//! - the `ConfigFiles` directory becomes overlays (and `repo.conf` a
//!   post-install script that sets the repository of the installed system).

use crate::project::{COMPRESSIONS, Overlay, Project, Repository, is_valid_name, parse_list};
use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};
use std::fs;
use std::path::{Path, PathBuf};

/// Packages the live image pipeline needs (kernel image and live initramfs).
const REQUIRED_PACKAGES: &[&str] = &["kernel", "mkinitcpio"];

/// Result of a conversion: the project and notes about settings that could
/// not be converted exactly.
#[derive(Debug)]
pub struct Conversion {
    pub project: Project,
    pub warnings: Vec<String>,
}

/// Reads a Pisiman XML file and converts it.
pub fn convert_file(path: &Path) -> Result<Conversion, String> {
    let xml = fs::read(path).map_err(|e| format!("Can not read {}: {e} !", path.display()))?;
    convert(&xml).map_err(|e| format!("{}: {e}", path.display()))
}

/// Converts a Pisiman project file into a project.
pub fn convert(xml: &[u8]) -> Result<Conversion, String> {
    let doc = parse(xml)?;
    let mut warnings = Vec::new();
    let mut project = Project::default();

    let title = doc.title.trim();
    if !title.is_empty() {
        project.name = title.to_string();
        if let Some(version) = version_from_title(title) {
            project.version = version;
        }
        project.volume_label = volume_label_from_title(title);
        project.output_iso = PathBuf::from(format!("{}.iso", iso_file_name(title)));
    }
    if !doc.work_dir.trim().is_empty() {
        project.work_dir = PathBuf::from(doc.work_dir.trim());
    }

    let compression = doc.compression.trim();
    if COMPRESSIONS.contains(&compression) {
        project.squashfs_compression = compression.to_string();
    } else if !compression.is_empty() {
        warnings.push(format!(
            "SquashFS compression '{compression}' is not supported, using '{}'.",
            project.squashfs_compression
        ));
    }

    let extra = doc.extra_params.trim();
    if !extra.is_empty() {
        project.kernel_cmdline = format!("{} {extra}", project.kernel_cmdline)
            .trim()
            .to_string();
    }

    // Repositories: the first one gets a plain name, others are numbered.
    project.repositories.clear();
    for uri in &doc.repo_uris {
        if project.repositories.iter().any(|r| &r.url == uri) {
            continue;
        }
        let name = match project.repositories.len() {
            0 => "pisi".to_string(),
            n => format!("pisi-{}", n + 1),
        };
        project.repositories.push(Repository::new(&name, uri));
    }
    if project.repositories.is_empty() {
        project.repositories = Project::default().repositories;
        warnings.push(format!(
            "No repository (repo_uri) found, using the default '{}'.",
            project.repositories[0].url
        ));
    }

    project.components = unique(doc.selections.iter().flat_map(|s| &s.components));
    project.packages = unique(doc.selections.iter().flat_map(|s| &s.packages));
    if project.components.is_empty() && project.packages.is_empty() {
        // Old files may only contain the resolved package list.
        project.packages = unique(doc.selections.iter().flat_map(|s| &s.all_packages));
    } else if doc.selections.iter().any(|s| !s.all_packages.is_empty()) {
        warnings.push(
            "The resolved package list (<Package>) was not copied; PiSi resolves the \
             dependencies of the selected components and packages again."
                .into(),
        );
    }
    for package in parse_list(&doc.plugin_package) {
        if !project.packages.contains(&package) {
            project.packages.push(package);
        }
    }
    let added: Vec<&str> = REQUIRED_PACKAGES
        .iter()
        .copied()
        .filter(|p| !project.packages.iter().any(|q| q == p))
        .collect();
    if !added.is_empty() {
        project.packages.extend(added.iter().map(|p| p.to_string()));
        warnings.push(format!(
            "Added packages needed by the live image: {}.",
            added.join(", ")
        ));
    }
    for name in project.components.iter().chain(&project.packages) {
        if !is_valid_name(name) {
            warnings.push(format!("Invalid component or package name '{name}'."));
        }
    }

    if doc.split_images {
        warnings.push(
            "Root, desktop and live CD image selections were merged into one live image.".into(),
        );
    }
    if doc.collections > 0 {
        warnings.push(format!(
            "{} package collection(s) were merged into one image.",
            doc.collections
        ));
    }
    if !doc.languages.is_empty() || !doc.default_language.is_empty() {
        warnings.push(format!(
            "Language selection (default '{}': {}) is not supported and was ignored.",
            doc.default_language,
            doc.languages.join(", ")
        ));
    }
    let kind = doc.kind.trim();
    match DISPLAY_MANAGER_TYPES
        .iter()
        .find(|(t, _)| t.eq_ignore_ascii_case(kind))
    {
        Some((_, dm)) => project.display_manager = dm.to_string(),
        None if kind.is_empty() || kind == "None" => {}
        None => warnings.push(format!(
            "Project type '{kind}' is not supported and was ignored."
        )),
    }

    let config_dir = doc.config_files.trim();
    if !config_dir.is_empty() {
        apply_config_files(&mut project, Path::new(config_dir), &mut warnings);
    }

    warnings.extend(project.validate());
    Ok(Conversion { project, warnings })
}

/// Pisiman project types (live display manager) and their equivalents.
const DISPLAY_MANAGER_TYPES: &[(&str, &str)] = &[
    ("Sddm", "sddm"),
    ("Lightdm", "lightdm"),
    ("Lxdm", "lxdm"),
    ("Mdm", "mdm"),
];

/// Pisiman configuration files copied into the image: (path in the
/// configuration directory, target directory).
const CONFIG_OVERLAYS: &[(&str, &str)] = &[
    ("default-settings/etc", "/etc"),
    ("default-settings/autostart", "/etc/skel/.config/autostart"),
    ("calamares", "/etc/calamares"),
    ("live/sudoers", "/etc/sudoers.d"),
    ("göruntu", "/usr/share/wallpapers/Maia/contents"),
];

/// Boot files of Pisiman's isolinux / miso / systemd-boot setup. Pisi ISO
/// Creator writes its own GRUB menu and initramfs hook instead.
const CONFIG_BOOT_FILES: &[&str] = &[
    "autorun",
    "efi",
    "efi_grub",
    "initcpio",
    "isolinux",
    "isomounts",
    "linux.preset",
    "mkinitcpio-live.conf",
];

/// Converts a Pisiman `ConfigFiles` directory. When the directory exists
/// only the present parts are used; otherwise the standard layout is assumed.
fn apply_config_files(project: &mut Project, dir: &Path, warnings: &mut Vec<String>) {
    let exists = dir.is_dir();
    if !exists {
        warnings.push(format!(
            "ConfigFiles directory {} was not found; the standard Pisiman layout was \
             assumed, check the overlays.",
            dir.display()
        ));
    }
    let present = |rel: &str| !exists || dir.join(rel).exists();

    for (rel, target) in CONFIG_OVERLAYS {
        if present(rel) {
            project.overlays.push(Overlay::new(dir.join(rel), *target));
        }
    }

    // live/<desktop>/.config and live/<desktop>/autostart go to the home
    // directory of the live user.
    let mut desktops: Vec<String> = if exists {
        fs::read_dir(dir.join("live"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "sudoers")
            .collect()
    } else {
        vec!["kde".into()]
    };
    desktops.sort();
    if !desktops.is_empty() && project.live_user.is_empty() {
        warnings.push("Live user settings were skipped because there is no live user.".into());
    } else {
        let home = Path::new("/home").join(&project.live_user);
        for desktop in &desktops {
            for (rel, target) in [
                (".config", home.join(".config")),
                ("autostart", home.join(".config/autostart")),
            ] {
                let rel = format!("live/{desktop}/{rel}");
                if present(&rel) {
                    project.overlays.push(Overlay::new(dir.join(rel), target));
                }
            }
        }
    }

    // repo.conf: repository of the installed system, replacing the
    // (often local) repositories the image was built from.
    match fs::read_to_string(dir.join("repo.conf")) {
        Ok(text) => {
            let url = text.trim();
            if crate::project::is_valid_url(url) {
                let mut script = String::from("# Repository from Pisiman repo.conf\n");
                for repo in &project.repositories {
                    script.push_str(&format!("pisi remove-repo {}\n", repo.name));
                }
                script.push_str(&format!("pisi add-repo --no-fetch pisi {url}\n"));
                project.post_install_script = script + &project.post_install_script;
            } else {
                warnings.push(format!("Invalid repository URL '{url}' in repo.conf."));
            }
        }
        Err(_) => warnings.push(format!(
            "{} could not be read; the build repositories stay in the image.",
            dir.join("repo.conf").display()
        )),
    }

    let boot: Vec<&str> = CONFIG_BOOT_FILES
        .iter()
        .copied()
        .filter(|f| exists && dir.join(f).exists())
        .collect();
    if !boot.is_empty() {
        warnings.push(format!(
            "Boot files from ConfigFiles ({}) were not used; Pisi ISO Creator generates \
             its own GRUB menu and live initramfs.",
            boot.join(", ")
        ));
    }
    if exists {
        let known: Vec<&str> = CONFIG_OVERLAYS
            .iter()
            .map(|(rel, _)| rel.split('/').next().unwrap_or(rel))
            .chain(CONFIG_BOOT_FILES.iter().copied())
            .chain(["live", "repo.conf"])
            .collect();
        let mut unknown: Vec<String> = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !known.contains(&n.as_str()))
            .collect();
        unknown.sort();
        if !unknown.is_empty() {
            warnings.push(format!(
                "Unknown entries in ConfigFiles were not used: {}.",
                unknown.join(", ")
            ));
        }
    }
}

/// A `PackageSelection`, `RootImagePackages`, `DesktopImagePackages` or
/// `LivecdImagePackages` element.
#[derive(Default, Debug)]
struct Selection {
    components: Vec<String>,
    packages: Vec<String>,
    all_packages: Vec<String>,
}

#[derive(Default, Debug)]
struct PisimanDoc {
    kind: String,
    compression: String,
    title: String,
    work_dir: String,
    config_files: String,
    plugin_package: String,
    extra_params: String,
    repo_uris: Vec<String>,
    selections: Vec<Selection>,
    split_images: bool,
    collections: usize,
    default_language: String,
    languages: Vec<String>,
}

const SELECTION_TAGS: &[&str] = &[
    "PackageSelection",
    "RootImagePackages",
    "DesktopImagePackages",
    "LivecdImagePackages",
];

fn parse(xml: &[u8]) -> Result<PisimanDoc, String> {
    let mut reader = Reader::from_reader(xml);
    let mut doc = PisimanDoc::default();
    let mut path: Vec<String> = Vec::new();
    let mut buf = Vec::new();
    let mut text = String::new();

    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| format!("Invalid XML at {}: {e} !", reader.buffer_position()))?;
        let (start, empty) = match &event {
            Event::Start(e) => (Some(e.clone()), false),
            Event::Empty(e) => (Some(e.clone()), true),
            _ => (None, false),
        };
        if let Some(e) = start {
            let name = e.local_name().as_ref().to_string();
            if path.is_empty() && !name.eq_ignore_ascii_case("PisiManProject") {
                return Err(format!(
                    "Root element is <{name}>, this is not a Pisiman project file!"
                ));
            }
            for attr in e.attributes().flatten() {
                let value = attr
                    .normalized_value(XmlVersion::Implicit1_0)
                    .map_err(|e| format!("Invalid attribute value: {e}"))?
                    .into_owned();
                match (path.len(), name.as_str(), attr.key.as_ref()) {
                    (0, _, "type") => doc.kind = value,
                    (0, _, "compression") => doc.compression = value,
                    (_, n, "repo_uri") if SELECTION_TAGS.contains(&n) => {
                        if !value.trim().is_empty() {
                            doc.repo_uris.push(value.trim().to_string());
                        }
                    }
                    (1, "LanguageSelection", "default_language") => doc.default_language = value,
                    _ => {}
                }
            }
            if SELECTION_TAGS.contains(&name.as_str()) {
                doc.selections.push(Selection::default());
                if name != "PackageSelection" {
                    doc.split_images = true;
                }
            }
            if name == "PackageCollection" {
                doc.collections += 1;
            }
            if !empty {
                path.push(name);
            }
            text.clear();
            buf.clear();
            continue;
        }
        match event {
            Event::Text(t) => text.push_str(&t.xml10_content()),
            Event::GeneralRef(r) => {
                if let Some(c) = r
                    .resolve_char_ref()
                    .map_err(|e| format!("Invalid character reference: {e}"))?
                {
                    text.push(c);
                } else {
                    text.push_str(match r.as_ref() {
                        "amp" => "&",
                        "lt" => "<",
                        "gt" => ">",
                        "quot" => "\"",
                        "apos" => "'",
                        _ => "",
                    });
                }
            }
            Event::CData(t) => text.push_str(t.as_ref()),
            Event::End(_) => {
                let value = text.trim().to_string();
                let p: Vec<&str> = path.iter().map(String::as_str).collect();
                match p.as_slice() {
                    [_, "Title"] => doc.title = value,
                    [_, "WorkDir"] => doc.work_dir = value,
                    [_, "ConfigFiles"] => doc.config_files = value,
                    [_, "PluginPackage"] => doc.plugin_package = value,
                    [_, "ExtraParameters"] => doc.extra_params = value,
                    [_, "LanguageSelection", "Language"] if !value.is_empty() => {
                        doc.languages.push(value)
                    }
                    [.., parent, item] if SELECTION_TAGS.contains(parent) && !value.is_empty() => {
                        if let Some(sel) = doc.selections.last_mut() {
                            match *item {
                                "SelectedComponent" => sel.components.push(value),
                                "SelectedPackage" => sel.packages.push(value),
                                "Package" => sel.all_packages.push(value),
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                path.pop();
                text.clear();
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    if doc.title.is_empty() && doc.selections.is_empty() && doc.kind.is_empty() {
        return Err("Not a Pisiman project file (no <PisiManProject> element)!".into());
    }
    Ok(doc)
}

/// Removes duplicates and keeps the first occurrence order.
fn unique<'a>(items: impl Iterator<Item = &'a String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in items {
        if !out.contains(item) {
            out.push(item.clone());
        }
    }
    out
}

/// First dotted number in the title, e.g. "2.2" in "Pisilinux-2.2-KDE".
fn version_from_title(title: &str) -> Option<String> {
    title
        .split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map(|s| s.trim_matches('.'))
        .find(|s| s.contains('.') && s.split('.').all(|p| !p.is_empty()))
        .map(str::to_string)
}

/// ISO9660 label: allowed characters only, at most 32 characters.
fn volume_label_from_title(title: &str) -> String {
    let label: String = title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(32)
        .collect();
    let label = label.trim_matches('_').to_string();
    if label.is_empty() {
        Project::default().volume_label
    } else {
        label
    }
}

/// The ISO file name Pisiman uses: spaces replaced by '-', no parentheses.
fn iso_file_name(title: &str) -> String {
    title
        .chars()
        .filter(|c| !matches!(c, '(' | ')' | '/'))
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" ?>
<PisiManProject type="Sddm" compression="xz">
    <Title>Pisilinux 2.2 KDE (x86_64)</Title>
    <WorkDir>/media/work</WorkDir>
    <ConfigFiles>/home/user/pisiman/config</ConfigFiles>
    <ExtraParameters>mudur=livecd</ExtraParameters>
    <PackageSelection repo_uri="file:///media/repo/pisi-index.xml.xz">
        <SelectedComponent>system.base</SelectedComponent>
        <SelectedComponent>x11.server</SelectedComponent>
        <SelectedPackage>kernel</SelectedPackage>
        <SelectedPackage>firefox</SelectedPackage>
        <Package>bash</Package>
        <Package>firefox</Package>
    </PackageSelection>
    <RootImagePackages>
        <SelectedComponent>system.base</SelectedComponent>
        <SelectedPackage>grub2</SelectedPackage>
    </RootImagePackages>
    <LanguageSelection default_language="tr_TR">
        <Language>tr_TR</Language>
        <Language>en_US</Language>
    </LanguageSelection>
</PisiManProject>
"#;

    #[test]
    fn converts_pisiman_project() {
        let c = convert(SAMPLE.as_bytes()).unwrap();
        let p = &c.project;
        assert_eq!(p.name, "Pisilinux 2.2 KDE (x86_64)");
        assert_eq!(p.version, "2.2");
        assert_eq!(p.volume_label, "Pisilinux_2_2_KDE__x86_64");
        assert_eq!(p.output_iso, PathBuf::from("Pisilinux-2.2-KDE-x86_64.iso"));
        assert_eq!(p.work_dir, PathBuf::from("/media/work"));
        assert_eq!(p.kernel_cmdline, "quiet splash mudur=livecd");
        assert_eq!(
            p.repositories,
            vec![Repository::new(
                "pisi",
                "file:///media/repo/pisi-index.xml.xz"
            )]
        );
        assert_eq!(p.components, vec!["system.base", "x11.server"]);
        assert_eq!(p.packages, vec!["kernel", "firefox", "grub2", "mkinitcpio"]);
        assert!(p.validate().is_empty(), "{:?}", p.validate());
        assert_eq!(p.display_manager, "sddm");
        assert_eq!(p.packages.last().unwrap(), "mkinitcpio");
        // The configuration directory does not exist here: standard layout.
        assert!(p.overlays.contains(&Overlay::new(
            "/home/user/pisiman/config/calamares",
            "/etc/calamares"
        )));
        assert!(p.overlays.contains(&Overlay::new(
            "/home/user/pisiman/config/live/kde/.config",
            "/home/pisi/.config"
        )));
        let w = c.warnings.join("\n");
        assert!(w.contains("mkinitcpio"));
        assert!(w.contains("tr_TR"));
        assert!(w.contains("was not found"));
        assert!(w.contains("merged"));
    }

    #[test]
    fn converts_existing_config_dir_and_plugin_package() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        for d in [
            "calamares/modules",
            "live/sudoers",
            "live/xfce/.config",
            "default-settings/etc",
            "initcpio/hooks",
            "efi",
            "extra",
        ] {
            fs::create_dir_all(config.join(d)).unwrap();
        }
        fs::write(
            config.join("repo.conf"),
            "https://example.org/pisi-index.xml.xz\n",
        )
        .unwrap();
        let xml = format!(
            r#"<PisiManProject type="Lxdm" compression="xz">
            <Title>Xfce</Title>
            <ConfigFiles>{}</ConfigFiles>
            <PluginPackage>pisi-branding</PluginPackage>
            <PackageSelection repo_uri="file:///repo/pisi-index.xml.xz">
                <SelectedComponent>system.base</SelectedComponent>
            </PackageSelection>
        </PisiManProject>"#,
            config.display()
        );
        let c = convert(xml.as_bytes()).unwrap();
        let p = &c.project;
        assert_eq!(p.display_manager, "lxdm");
        assert!(p.packages.contains(&"pisi-branding".to_string()));
        let targets: Vec<_> = p
            .overlays
            .iter()
            .map(|o| o.target.display().to_string())
            .collect();
        assert_eq!(
            targets,
            vec![
                "/etc",
                "/etc/calamares",
                "/etc/sudoers.d",
                "/home/pisi/.config"
            ]
        );
        assert_eq!(
            p.post_install_script,
            "# Repository from Pisiman repo.conf\npisi remove-repo pisi\n\
             pisi add-repo --no-fetch pisi https://example.org/pisi-index.xml.xz\n"
        );
        let w = c.warnings.join("\n");
        assert!(w.contains("efi, initcpio"), "{w}");
        assert!(w.contains("not used: extra"), "{w}");
        assert!(p.validate().is_empty(), "{:?}", p.validate());
    }

    #[test]
    fn uses_resolved_list_without_selection() {
        let xml = r#"<PisiManProject type="None" compression="lz4">
            <Title>Test</Title>
            <PackageSelection repo_uri="https://example.org/pisi-index.xml.xz">
                <Package>bash</Package><Package>kernel</Package>
            </PackageSelection>
        </PisiManProject>"#;
        let c = convert(xml.as_bytes()).unwrap();
        assert_eq!(c.project.packages, vec!["bash", "kernel", "mkinitcpio"]);
        assert_eq!(c.project.squashfs_compression, "lz4");
        assert_eq!(c.project.version, Project::default().version);
    }

    #[test]
    fn rejects_other_xml() {
        assert!(convert(b"<PISI><Package/></PISI>").is_err());
        assert!(convert(b"<PisiManProject").is_err());
    }

    #[test]
    fn title_helpers() {
        assert_eq!(
            version_from_title("Pisilinux-2.2-StayHome-UEFI_KDE_x86_64"),
            Some("2.2".into())
        );
        assert_eq!(version_from_title("Pisilinux-lumia-x86_64"), None);
        assert_eq!(
            volume_label_from_title("Pisilinux-lumıa-x86_64"),
            "Pisilinux-lum_a-x86_64"
        );
        assert_eq!(volume_label_from_title("   "), "PISI_LIVE");
    }
}
