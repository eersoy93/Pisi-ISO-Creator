//! PiSi package manager integration: command construction and repository
//! index (`pisi-index.xml` / `pisi-index.xml.xz`) parsing.

use crate::command::Cmd;
use quick_xml::Reader;
use quick_xml::events::Event;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

/// Builds `pisi` command lines that operate on a target root directory
/// (`pisi -D <root> ...`) or inside a chroot.
#[derive(Clone, Debug)]
pub struct Pisi {
    root: PathBuf,
}

impl Pisi {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn base(&self, subcommand: &str) -> Cmd {
        Cmd::new("pisi")
            .arg("--yes-all")
            .arg(format!("--destdir={}", self.root.display()))
            .arg(subcommand)
    }

    pub fn add_repo(&self, name: &str, url: &str) -> Cmd {
        self.base("add-repo").arg(name).arg(url)
    }

    pub fn update_repo(&self) -> Cmd {
        self.base("update-repo")
    }

    /// Installs packages without running COMAR scripts; they are executed
    /// later inside the chroot by `configure-pending`.
    pub fn install(&self, packages: &[String]) -> Cmd {
        self.base("install")
            .arg("--ignore-comar")
            .arg("--ignore-file-conflicts")
            .args(packages)
    }

    pub fn install_components(&self, components: &[String]) -> Cmd {
        let mut cmd = self
            .base("install")
            .arg("--ignore-comar")
            .arg("--ignore-file-conflicts");
        for component in components {
            cmd = cmd.arg("--component").arg(component);
        }
        cmd
    }

    pub fn remove(&self, packages: &[String]) -> Cmd {
        self.base("remove").arg("--ignore-comar").args(packages)
    }

    pub fn upgrade(&self) -> Cmd {
        self.base("upgrade")
            .arg("--ignore-comar")
            .arg("--ignore-file-conflicts")
    }

    /// `pisi configure-pending` executed inside the chroot, so that COMAR
    /// post-install scripts run against the target system.
    pub fn configure_pending_in_chroot(&self, package: Option<&str>) -> Cmd {
        let cmd = chroot(&self.root, "/usr/bin/pisi")
            .arg("--yes-all")
            .arg("configure-pending");
        match package {
            Some(p) => cmd.arg(p),
            None => cmd,
        }
    }

    /// Deletes downloaded `.pisi` files from the target package cache.
    pub fn delete_cache_in_chroot(&self) -> Cmd {
        chroot(&self.root, "/usr/bin/pisi")
            .arg("--yes-all")
            .arg("delete-cache")
            .allow_failure()
    }
}

/// `chroot <root> <program>`.
pub fn chroot(root: &Path, program: &str) -> Cmd {
    Cmd::new("chroot").arg(root.as_os_str()).arg(program)
}

/// A binary package found in a PiSi repository index.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackageInfo {
    pub name: String,
    pub version: String,
    pub release: String,
    pub summary: String,
    pub component: String,
    pub dependencies: Vec<String>,
    pub installed_size: u64,
}

impl PackageInfo {
    pub fn label(&self) -> String {
        let mut s = self.name.clone();
        if !self.version.is_empty() {
            s.push_str(&format!(" {}", self.version));
            if !self.release.is_empty() {
                s.push_str(&format!("-{}", self.release));
            }
        }
        if !self.summary.is_empty() {
            s.push_str(&format!(" — {}", self.summary));
        }
        s
    }

    pub fn details(&self) -> String {
        let mut s = format!("Name: {}\n", self.name);
        if !self.version.is_empty() {
            s.push_str(&format!("Version: {}-{}\n", self.version, self.release));
        }
        if !self.component.is_empty() {
            s.push_str(&format!("Component: {}\n", self.component));
        }
        if self.installed_size > 0 {
            s.push_str(&format!(
                "Installed size: {:.1} MiB\n",
                self.installed_size as f64 / 1024.0 / 1024.0
            ));
        }
        if !self.summary.is_empty() {
            s.push_str(&format!("Summary: {}\n", self.summary));
        }
        if !self.dependencies.is_empty() {
            s.push_str(&format!("Dependencies: {}\n", self.dependencies.join(", ")));
        }
        s
    }
}

/// Contents of a PiSi repository index.
#[derive(Clone, Debug, Default)]
pub struct RepoIndex {
    pub packages: Vec<PackageInfo>,
    pub components: Vec<String>,
}

impl RepoIndex {
    /// Case insensitive search on name, summary and component.
    pub fn search(&self, query: &str) -> Vec<&PackageInfo> {
        let q = query.trim().to_lowercase();
        let mut found: Vec<&PackageInfo> = self
            .packages
            .iter()
            .filter(|p| {
                q.is_empty()
                    || p.name.to_lowercase().contains(&q)
                    || p.summary.to_lowercase().contains(&q)
                    || p.component.to_lowercase() == q
            })
            .collect();
        // Exact and prefix matches first.
        found.sort_by_key(|p| {
            let n = p.name.to_lowercase();
            (n != q, !n.starts_with(&q), n)
        });
        found
    }
}

/// Maximum size accepted for a (decompressed) index, protects against
/// decompression bombs.
const MAX_INDEX_SIZE: u64 = 512 * 1024 * 1024;

/// Loads an index from a local path or an http(s) URL. `.xz` compressed
/// indexes are detected automatically.
pub fn load_index(source: &str) -> Result<RepoIndex, String> {
    let source = source.trim();
    let raw = if source.starts_with("http://") || source.starts_with("https://") {
        download(source)?
    } else {
        let path = source.strip_prefix("file://").unwrap_or(source);
        let file = std::fs::File::open(path).map_err(|e| format!("cannot open {path}: {e}"))?;
        let mut data = Vec::new();
        file.take(MAX_INDEX_SIZE)
            .read_to_end(&mut data)
            .map_err(|e| format!("cannot read {path}: {e}"))?;
        data
    };
    parse_index(&decompress(raw)?)
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    // Use the operating system trust store, so that distribution and
    // locally installed CA certificates are honoured.
    let tls = ureq::tls::TlsConfig::builder()
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_global(Some(std::time::Duration::from_secs(300)))
        .build()
        .into();
    let mut response = agent
        .get(url)
        .call()
        .map_err(|e| format!("Can not download {url}: {e}"))?;
    response
        .body_mut()
        .with_config()
        .limit(MAX_INDEX_SIZE)
        .read_to_vec()
        .map_err(|e| format!("Can not download {url}: {e}"))
}

const XZ_MAGIC: &[u8] = &[0xFD, b'7', b'z', b'X', b'Z', 0x00];

fn decompress(raw: Vec<u8>) -> Result<Vec<u8>, String> {
    if !raw.starts_with(XZ_MAGIC) {
        return Ok(raw);
    }
    let mut out = LimitedWriter(Vec::new());
    let mut input = BufReader::new(raw.as_slice());
    lzma_rs::xz_decompress(&mut input, &mut out)
        .map_err(|e| format!("Can not decompress index: {e}"))?;
    Ok(out.0)
}

/// Writer that fails once more than [`MAX_INDEX_SIZE`] bytes are written.
struct LimitedWriter(Vec<u8>);

impl Write for LimitedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if (self.0.len() + buf.len()) as u64 > MAX_INDEX_SIZE {
            return Err(std::io::Error::other("Index is too large!"));
        }
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Parses a PiSi index. Both binary indexes (top level `<Package>`
/// elements) and source indexes (`<SpecFile>` with `<Source>` and binary
/// `<Package>` children) are supported.
pub fn parse_index(xml: &[u8]) -> Result<RepoIndex, String> {
    let mut reader = Reader::from_reader(xml);

    let mut index = RepoIndex::default();
    let mut path: Vec<String> = Vec::new();
    let mut buf = Vec::new();

    let mut pkg = PackageInfo::default();
    // Values from <SpecFile>/<Source> and <SpecFile>/<History>, applied to
    // every binary package of the spec file.
    let mut spec_packages: Vec<PackageInfo> = Vec::new();
    let mut spec_component = String::new();
    let mut spec_summary = String::new();
    let mut history = (String::new(), String::new()); // (version, release)
    let mut in_first_update = false;
    let mut seen_update = false;
    let mut lang_en = true;
    let mut text = String::new();

    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| format!("Invalid index XML at {}: {e}", reader.buffer_position()))?;
        match event {
            Event::Start(e) => {
                let name = e.local_name().as_ref().to_string();
                lang_en = true;
                for attr in e.attributes().flatten() {
                    let key = attr.key.as_ref();
                    let value = attr.value.to_string();
                    if key == "xml:lang" {
                        lang_en = value == "en";
                    } else if key == "release" && name == "Update" && !seen_update {
                        history.1 = value;
                    }
                }
                if name == "Update" {
                    in_first_update = !seen_update;
                    seen_update = true;
                }
                if name == "Package" && is_package_path(&path) {
                    pkg = PackageInfo::default();
                }
                path.push(name);
                if path.len() == 2 && path[1] == "SpecFile" {
                    spec_packages.clear();
                    spec_component.clear();
                    spec_summary.clear();
                    history = (String::new(), String::new());
                    seen_update = false;
                }
                if path.len() == 2 && path[1] == "Package" {
                    history = (String::new(), String::new());
                    seen_update = false;
                }
                text.clear();
            }
            Event::Text(t) => {
                text.push_str(&t.xml10_content());
            }
            Event::GeneralRef(r) => {
                if let Some(c) = r.resolve_char_ref().map_err(|e| e.to_string())? {
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
                let p: Vec<&str> = path.iter().map(String::as_str).collect();
                let value = text.trim().to_string();
                match p.as_slice() {
                    // Binary index.
                    ["PISI", "Package", "Name"] => pkg.name = value,
                    ["PISI", "Package", "Summary"] if lang_en || pkg.summary.is_empty() => {
                        pkg.summary = value
                    }
                    ["PISI", "Package", "PartOf"] => pkg.component = value,
                    ["PISI", "Package", "InstalledSize"] => {
                        pkg.installed_size = value.parse().unwrap_or(0)
                    }
                    ["PISI", "Package", "RuntimeDependencies", "Dependency"] => {
                        pkg.dependencies.push(value)
                    }
                    ["PISI", "Package", "History", "Update", "Version"] if in_first_update => {
                        history.0 = value
                    }
                    ["PISI", "Package"] => {
                        let mut done = std::mem::take(&mut pkg);
                        (done.version, done.release) = history.clone();
                        if !done.name.is_empty() {
                            index.packages.push(done);
                        }
                    }
                    // Source index.
                    ["PISI", "SpecFile", "Source", "PartOf"] => spec_component = value,
                    ["PISI", "SpecFile", "Source", "Summary"]
                        if lang_en || spec_summary.is_empty() =>
                    {
                        spec_summary = value
                    }
                    ["PISI", "SpecFile", "Package", "Name"] => pkg.name = value,
                    ["PISI", "SpecFile", "Package", "Summary"]
                        if lang_en || pkg.summary.is_empty() =>
                    {
                        pkg.summary = value
                    }
                    ["PISI", "SpecFile", "Package", "PartOf"] => pkg.component = value,
                    [
                        "PISI",
                        "SpecFile",
                        "Package",
                        "RuntimeDependencies",
                        "Dependency",
                    ] => pkg.dependencies.push(value),
                    ["PISI", "SpecFile", "Package"] => {
                        spec_packages.push(std::mem::take(&mut pkg));
                    }
                    ["PISI", "SpecFile", "History", "Update", "Version"] if in_first_update => {
                        history.0 = value
                    }
                    ["PISI", "SpecFile"] => {
                        for mut p in spec_packages.drain(..) {
                            if p.component.is_empty() {
                                p.component = spec_component.clone();
                            }
                            if p.summary.is_empty() {
                                p.summary = spec_summary.clone();
                            }
                            (p.version, p.release) = history.clone();
                            if !p.name.is_empty() {
                                index.packages.push(p);
                            }
                        }
                    }
                    ["PISI", "Component", "Name"] => index.components.push(value),
                    [.., "Update"] => in_first_update = false,
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
    if index.packages.is_empty() && index.components.is_empty() {
        return Err("No packages found; is this a PiSi index?".into());
    }
    index.packages.sort_by(|a, b| a.name.cmp(&b.name));
    index.packages.dedup_by(|a, b| a.name == b.name);
    index.components.sort();
    index.components.dedup();
    Ok(index)
}

fn is_package_path(parent: &[String]) -> bool {
    matches!(
        parent
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice(),
        ["PISI"] | ["PISI", "SpecFile"]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const BINARY: &str = r#"<PISI>
  <Distribution><SourceName>PisiLinux</SourceName><Obsoletes><Package>old</Package></Obsoletes></Distribution>
  <Package>
    <Name>nano</Name>
    <Summary xml:lang="tr">Metin d&#252;zenleyici</Summary>
    <Summary xml:lang="en">Small &amp; friendly editor</Summary>
    <PartOf>editor.console</PartOf>
    <RuntimeDependencies><Dependency>ncurses</Dependency><Dependency release="2">file</Dependency></RuntimeDependencies>
    <History>
      <Update release="12"><Date>2024-01-01</Date><Version>7.2</Version><Name>Packager</Name></Update>
      <Update release="11"><Version>7.1</Version></Update>
    </History>
    <InstalledSize>2097152</InstalledSize>
  </Package>
  <Component><Name>editor.console</Name></Component>
</PISI>"#;

    const SOURCE: &str = r#"<PISI>
  <SpecFile>
    <Source>
      <Name>phodav</Name>
      <Packager><Name>Someone</Name></Packager>
      <PartOf>server.misc</PartOf>
      <Summary xml:lang="en">WebDav server</Summary>
    </Source>
    <Package><Name>phodav</Name><RuntimeDependencies><Dependency>glib2</Dependency></RuntimeDependencies></Package>
    <Package><Name>phodav-devel</Name><Summary xml:lang="en">Development files</Summary><PartOf>programming.devel</PartOf></Package>
    <History>
      <Update release="2"><Version>3.0</Version><Name>A</Name></Update>
      <Update release="1"><Version>2.5</Version></Update>
    </History>
  </SpecFile>
</PISI>"#;

    #[test]
    fn parses_binary_index() {
        let index = parse_index(BINARY.as_bytes()).unwrap();
        assert_eq!(index.packages.len(), 1);
        let p = &index.packages[0];
        assert_eq!(p.name, "nano");
        assert_eq!(p.version, "7.2");
        assert_eq!(p.release, "12");
        assert_eq!(p.summary, "Small & friendly editor");
        assert_eq!(p.component, "editor.console");
        assert_eq!(p.dependencies, vec!["ncurses", "file"]);
        assert_eq!(p.installed_size, 2097152);
        assert_eq!(index.components, vec!["editor.console"]);
        assert!(p.label().starts_with("nano 7.2-12"));
        assert!(p.details().contains("2.0 MiB"));
    }

    #[test]
    fn parses_source_index() {
        let index = parse_index(SOURCE.as_bytes()).unwrap();
        let names: Vec<_> = index.packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["phodav", "phodav-devel"]);
        let p = &index.packages[0];
        assert_eq!((p.version.as_str(), p.release.as_str()), ("3.0", "2"));
        assert_eq!(p.summary, "WebDav server");
        assert_eq!(p.component, "server.misc");
        assert_eq!(p.dependencies, vec!["glib2"]);
        assert_eq!(index.packages[1].component, "programming.devel");
    }

    #[test]
    fn rejects_non_index() {
        assert!(parse_index(b"<html></html>").is_err());
        assert!(parse_index(b"<PISI><Package>").is_err());
    }

    #[test]
    fn loads_xz_index_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pisi-index.xml.xz");
        let mut compressed = Vec::new();
        lzma_rs::xz_compress(&mut BINARY.as_bytes(), &mut compressed).unwrap();
        std::fs::write(&path, compressed).unwrap();
        let index = load_index(path.to_str().unwrap()).unwrap();
        assert_eq!(index.packages[0].name, "nano");
    }

    #[test]
    fn search_ranks_exact_matches_first() {
        let index = RepoIndex {
            packages: vec![
                PackageInfo {
                    name: "libnano".into(),
                    ..Default::default()
                },
                PackageInfo {
                    name: "nano".into(),
                    ..Default::default()
                },
                PackageInfo {
                    name: "vim".into(),
                    summary: "editor".into(),
                    ..Default::default()
                },
            ],
            components: vec![],
        };
        let found: Vec<_> = index
            .search("NANO")
            .iter()
            .map(|p| p.name.clone())
            .collect();
        assert_eq!(found, vec!["nano", "libnano"]);
        assert_eq!(index.search("edit").len(), 1);
        assert_eq!(index.search("").len(), 3);
    }

    #[test]
    fn builds_pisi_commands() {
        let pisi = Pisi::new("/work/rootfs");
        assert_eq!(
            pisi.add_repo("main", "https://x/pisi-index.xml.xz")
                .to_string(),
            "pisi --yes-all --destdir=/work/rootfs add-repo main https://x/pisi-index.xml.xz"
        );
        assert_eq!(
            pisi.install_components(&["system.base".into()]).to_string(),
            "pisi --yes-all --destdir=/work/rootfs install --ignore-comar --ignore-file-conflicts --component system.base"
        );
        assert_eq!(
            pisi.configure_pending_in_chroot(Some("baselayout"))
                .to_string(),
            "chroot /work/rootfs /usr/bin/pisi --yes-all configure-pending baselayout"
        );
    }
}
