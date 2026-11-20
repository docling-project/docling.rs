//! The taxonomy behind an XBRL instance — XBRL's "discoverable taxonomy set"
//! (DTS): the schemas the instance's `link:schemaRef`s name, whatever those
//! import, and the linkbases they refer to. docling's `XbrlDocumentBackend`
//! has arelle load it (#466); what the conversion actually consumes is small
//! enough to read directly: each concept's declared type (a `textBlockItemType`
//! fact is a block of HTML), the qualified name a locator's `id` fragment
//! resolves to, and the parent-child and summation-item arcs the fact graph's
//! hierarchy is built from.
//!
//! Documents are found the way arelle's offline mode finds them: a relative
//! reference is a file under the taxonomy directory (the instance's own
//! directory when none is given), an absolute `http(s)` one is looked up in
//! the taxonomy packages (`.zip`) in that directory through their
//! `META-INF/catalog.xml` `rewriteURI` mappings. Nothing is ever fetched from
//! the network. A document that cannot be found is skipped: a locator to it
//! falls back to the SEC's `prefix_Name` id convention, and a fact of an
//! unknown concept is typed by shape (see [`super::xbrl`]) — the conversion
//! degrades to fewer hierarchy links rather than failing, where arelle
//! refuses to load an incomplete DTS.

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use roxmltree::{Document, Node, ParsingOptions};
use zip::ZipArchive;

pub(crate) const XS_NS: &str = "http://www.w3.org/2001/XMLSchema";
pub(crate) const LINK_NS: &str = "http://www.xbrl.org/2003/linkbase";
pub(crate) const XLINK_NS: &str = "http://www.w3.org/1999/xlink";
const PARENT_CHILD: &str = "http://www.xbrl.org/2003/arcrole/parent-child";
const SUMMATION_ITEM: &str = "http://www.xbrl.org/2003/arcrole/summation-item";

/// Documents a DTS may reach before discovery stops: the SEC base taxonomies
/// an annual report imports come to a few dozen.
const MAX_DOCUMENTS: usize = 512;
/// The largest schema or linkbase read (the US GAAP element schema is 5 MB).
const MAX_DOCUMENT_BYTES: u64 = 64 << 20;

/// A concept as its schema declares it.
#[derive(Debug, Clone)]
pub(crate) struct Concept {
    pub name: String,
    /// The prefix the declaring schema binds to its target namespace — what
    /// arelle prints a concept's qualified name with.
    pub prefix: Option<String>,
    /// The local name of the concept's `type` (`monetaryItemType`,
    /// `textBlockItemType`, …); empty when it declares none inline.
    pub type_name: String,
}

impl Concept {
    pub fn qname(&self) -> String {
        match &self.prefix {
            Some(p) => format!("{p}:{}", self.name),
            None => self.name.clone(),
        }
    }
}

/// One presentation or calculation arc, its ends already resolved to
/// qualified concept names.
#[derive(Debug, Clone)]
pub(crate) struct Arc {
    pub role: String,
    pub from: String,
    pub to: String,
    pub order: f64,
    pub priority: i64,
    pub prohibited: bool,
    /// A calculation arc's `weight`, as written.
    pub weight: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct Taxonomy {
    /// (namespace, local name) → concept.
    concepts: HashMap<(String, String), Concept>,
    /// `<document url>#<id>` → the concept's (namespace, local name).
    ids: HashMap<String, (String, String)>,
    /// Arcs in discovery order (arelle's base-set order), before the
    /// prohibition/priority resolution [`effective`] applies.
    pub presentation: Vec<Arc>,
    pub calculation: Vec<Arc>,
}

impl Taxonomy {
    /// Discover the DTS reachable from `schema_refs` (the instance's
    /// `link:schemaRef` hrefs), reading documents under `dir`.
    pub fn discover(dir: Option<&Path>, schema_refs: &[&str]) -> Taxonomy {
        let mut loader = Loader {
            resolver: Resolver::open(dir),
            seen: HashSet::new(),
            taxonomy: Taxonomy::default(),
        };
        for href in schema_refs {
            loader.load(&join_url("", href));
        }
        loader.taxonomy
    }

    pub fn concept(&self, namespace: &str, name: &str) -> Option<&Concept> {
        self.concepts
            .get(&(namespace.to_string(), name.to_string()))
    }
}

/// arelle's `ModelRelationshipSet.modelRelationships` over one arcrole: of
/// the arcs equivalent to one another (same extended-link role, ends and
/// order) the one with the highest priority stands — a prohibiting arc wins a
/// tie — and prohibited arcs then drop out; what remains is sorted by `order`,
/// a stable sort that keeps discovery order among equals.
pub(crate) fn effective(arcs: &[Arc]) -> Vec<&Arc> {
    let mut index: HashMap<(&str, &str, &str, u64), usize> = HashMap::new();
    let mut kept: Vec<&Arc> = Vec::new();
    for arc in arcs {
        let key = (
            arc.role.as_str(),
            arc.from.as_str(),
            arc.to.as_str(),
            arc.order.to_bits(),
        );
        match index.get(&key) {
            Some(&i) => {
                let held = kept[i];
                if arc.priority > held.priority
                    || (arc.priority == held.priority && arc.prohibited && !held.prohibited)
                {
                    kept[i] = arc;
                }
            }
            None => {
                index.insert(key, kept.len());
                kept.push(arc);
            }
        }
    }
    kept.retain(|a| !a.prohibited);
    kept.sort_by(|a, b| {
        a.order
            .partial_cmp(&b.order)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    kept
}

struct Loader {
    resolver: Resolver,
    seen: HashSet<String>,
    taxonomy: Taxonomy,
}

impl Loader {
    /// Load one document and everything it refers to, depth first in
    /// document order — the order arelle discovers a DTS in, which is the
    /// order its relationship sets keep arcs of equal `order` in.
    fn load(&mut self, url: &str) {
        if self.seen.len() >= MAX_DOCUMENTS || !self.seen.insert(url.to_string()) {
            return;
        }
        let Some(bytes) = self.resolver.read(url) else {
            return;
        };
        let text = String::from_utf8_lossy(&bytes);
        if super::xml_depth::check(&text, "xbrl taxonomy").is_err() {
            return;
        }
        let opts = ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        };
        let Ok(dom) = Document::parse_with_options(&text, opts) else {
            return;
        };
        let root = dom.root_element();
        match (root.tag_name().namespace(), root.tag_name().name()) {
            (Some(XS_NS), "schema") => self.schema(url, root),
            (Some(LINK_NS), "linkbase") => self.linkbase(url, root),
            _ => {}
        }
    }

    fn schema(&mut self, url: &str, root: Node) {
        let namespace = root.attribute("targetNamespace").unwrap_or("").to_string();
        let prefix = root.lookup_prefix(&namespace).map(str::to_string);
        for child in root.children().filter(Node::is_element) {
            match (child.tag_name().namespace(), child.tag_name().name()) {
                (Some(XS_NS), "import" | "include" | "redefine") => {
                    if let Some(location) = child.attribute("schemaLocation") {
                        self.load(&join_url(url, location));
                    }
                }
                (Some(XS_NS), "element") => {
                    let Some(name) = child.attribute("name") else {
                        continue;
                    };
                    let key = (namespace.clone(), name.to_string());
                    if let Some(id) = child.attribute("id") {
                        self.taxonomy.ids.insert(format!("{url}#{id}"), key.clone());
                    }
                    let type_name = child
                        .attribute("type")
                        .map(|t| t.rsplit(':').next().unwrap_or(t))
                        .unwrap_or("");
                    self.taxonomy.concepts.insert(
                        key,
                        Concept {
                            name: name.to_string(),
                            prefix: prefix.clone(),
                            type_name: type_name.to_string(),
                        },
                    );
                }
                (Some(XS_NS), "annotation") => {
                    let appinfos = child
                        .children()
                        .filter(|n| n.is_element() && n.tag_name().name() == "appinfo");
                    for appinfo in appinfos {
                        for item in appinfo.children().filter(Node::is_element) {
                            match (item.tag_name().namespace(), item.tag_name().name()) {
                                (Some(LINK_NS), "linkbaseRef") => {
                                    if let Some(href) = item.attribute((XLINK_NS, "href")) {
                                        self.load(&join_url(url, href));
                                    }
                                }
                                (Some(LINK_NS), "linkbase") => self.linkbase(url, item),
                                _ => {}
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn linkbase(&mut self, url: &str, root: Node) {
        for link in root.children().filter(Node::is_element) {
            if link.tag_name().namespace() != Some(LINK_NS)
                || !matches!(
                    link.tag_name().name(),
                    "presentationLink" | "calculationLink"
                )
            {
                continue;
            }
            let role = link.attribute((XLINK_NS, "role")).unwrap_or("").to_string();
            // Locators first: a locator's document is loaded on sight (arelle
            // discovers the US GAAP schema through the extension's locators),
            // and its fragment resolves to the concept the schema declares
            // under that id — or, when the schema is out of reach, to the
            // `prefix_Name` id every SEC taxonomy follows.
            let mut locators: HashMap<&str, String> = HashMap::new();
            for loc in link
                .children()
                .filter(|n| n.is_element() && n.tag_name().name() == "loc")
            {
                let (Some(href), Some(label)) = (
                    loc.attribute((XLINK_NS, "href")),
                    loc.attribute((XLINK_NS, "label")),
                ) else {
                    continue;
                };
                let (document, fragment) = href.split_once('#').unwrap_or((href, ""));
                let target = if document.is_empty() {
                    url.to_string()
                } else {
                    join_url(url, document)
                };
                self.load(&target);
                let qname = self
                    .taxonomy
                    .ids
                    .get(&format!("{target}#{fragment}"))
                    .and_then(|key| self.taxonomy.concepts.get(key))
                    .map(Concept::qname)
                    .unwrap_or_else(|| fragment.replacen('_', ":", 1));
                locators.insert(label, qname);
            }
            for arc in link.children().filter(Node::is_element) {
                let arcrole = arc.attribute((XLINK_NS, "arcrole")).unwrap_or("");
                let bucket = match arcrole {
                    PARENT_CHILD => &mut self.taxonomy.presentation,
                    SUMMATION_ITEM => &mut self.taxonomy.calculation,
                    _ => continue,
                };
                let (Some(from), Some(to)) = (
                    arc.attribute((XLINK_NS, "from"))
                        .and_then(|l| locators.get(l)),
                    arc.attribute((XLINK_NS, "to"))
                        .and_then(|l| locators.get(l)),
                ) else {
                    continue;
                };
                bucket.push(Arc {
                    role: role.clone(),
                    from: from.clone(),
                    to: to.clone(),
                    order: arc
                        .attribute("order")
                        .and_then(|o| o.trim().parse().ok())
                        .unwrap_or(1.0),
                    priority: arc
                        .attribute("priority")
                        .and_then(|p| p.trim().parse().ok())
                        .unwrap_or(0),
                    prohibited: arc.attribute("use") == Some("prohibited"),
                    weight: arc.attribute("weight").map(str::to_string),
                });
            }
        }
    }
}

/// Where documents come from: files under the taxonomy directory, and
/// entries of the taxonomy packages found there.
struct Resolver {
    dir: Option<PathBuf>,
    packages: Vec<TaxonomyPackage>,
}

struct TaxonomyPackage {
    archive: ZipArchive<Cursor<Vec<u8>>>,
    /// The catalog's `rewriteURI` entries: URL prefix → archive directory.
    rewrites: Vec<(String, String)>,
}

impl Resolver {
    fn open(dir: Option<&Path>) -> Resolver {
        let mut packages = Vec::new();
        if let Some(dir) = dir {
            let mut zips: Vec<PathBuf> = std::fs::read_dir(dir)
                .map(|entries| {
                    entries
                        .filter_map(Result::ok)
                        .map(|e| e.path())
                        .filter(|p| {
                            p.is_file()
                                && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip"))
                        })
                        .collect()
                })
                .unwrap_or_default();
            zips.sort();
            for path in zips {
                if let Some(package) = TaxonomyPackage::open(&path) {
                    packages.push(package);
                }
            }
        }
        Resolver {
            dir: dir.map(Path::to_path_buf),
            packages,
        }
    }

    fn read(&mut self, url: &str) -> Option<Vec<u8>> {
        if is_remote(url) {
            return self
                .packages
                .iter_mut()
                .find_map(|package| package.read(url));
        }
        let dir = self.dir.as_ref()?;
        let path = confined(dir, url)?;
        let len = std::fs::metadata(&path).ok()?.len();
        (len <= MAX_DOCUMENT_BYTES)
            .then(|| std::fs::read(&path).ok())
            .flatten()
    }
}

/// A relative taxonomy reference as a file inside `dir`, or `None` when it
/// could name anything outside it (#1192, the local counterpart of
/// docling#5406's web-cache confinement). The check used to be lexical and
/// `/`-only: on Windows `C:/Windows/win.ini` made `Path::join` replace the
/// directory, and `..\..\x.xsd` — one `/`-segment — climbed out of it. Now,
/// on every platform alike: `\` is a separator; a root, a drive or UNC
/// prefix, a `name:` first segment and any `..` component are refused; and
/// the canonical path (symlinks resolved) must lie under the canonical
/// directory, as `images.rs`' `confined_local_path` requires.
fn confined(dir: &Path, url: &str) -> Option<PathBuf> {
    let rel = url.replace('\\', "/");
    if rel.starts_with('/') || rel.split('/').next().is_some_and(|s| s.contains(':')) {
        return None;
    }
    if rel.split('/').any(|s| s == "..") {
        return None;
    }
    let rel = Path::new(&rel);
    if rel.is_absolute()
        || rel.has_root()
        || rel.components().any(|c| {
            matches!(
                c,
                std::path::Component::Prefix(_)
                    | std::path::Component::RootDir
                    | std::path::Component::ParentDir
            )
        })
    {
        return None;
    }
    let base = dir.canonicalize().ok()?;
    let full = base.join(rel).canonicalize().ok()?;
    full.starts_with(&base).then_some(full)
}

impl TaxonomyPackage {
    fn open(path: &Path) -> Option<TaxonomyPackage> {
        let bytes = std::fs::read(path).ok()?;
        let mut archive = ZipArchive::new(Cursor::new(bytes)).ok()?;
        let catalogs: Vec<String> = archive
            .file_names()
            .flatten()
            .filter(|n| n.ends_with("META-INF/catalog.xml"))
            .map(|n| n.into_owned())
            .collect();
        let mut rewrites = Vec::new();
        for name in catalogs {
            let Some(text) = read_entry(&mut archive, &name) else {
                continue;
            };
            let text = String::from_utf8_lossy(&text);
            let Ok(dom) = Document::parse(&text) else {
                continue;
            };
            let base = dirname(&name);
            for entry in dom
                .descendants()
                .filter(|n| n.is_element() && n.tag_name().name() == "rewriteURI")
            {
                if let (Some(start), Some(prefix)) = (
                    entry.attribute("uriStartString"),
                    entry.attribute("rewritePrefix"),
                ) {
                    rewrites.push((
                        start.to_string(),
                        normalize_path(&format!("{base}/{prefix}")),
                    ));
                }
            }
        }
        Some(TaxonomyPackage { archive, rewrites })
    }

    fn read(&mut self, url: &str) -> Option<Vec<u8>> {
        for (start, prefix) in &self.rewrites {
            if let Some(rest) = url.strip_prefix(start.as_str()) {
                let name = normalize_path(&format!("{prefix}/{rest}"));
                if let Some(bytes) = read_entry(&mut self.archive, &name) {
                    return Some(bytes);
                }
            }
        }
        None
    }
}

fn read_entry(archive: &mut ZipArchive<Cursor<Vec<u8>>>, name: &str) -> Option<Vec<u8>> {
    let mut entry = archive.by_name(name).ok()?;
    if entry.size() > MAX_DOCUMENT_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

fn is_remote(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn dirname(path: &str) -> &str {
    path.rfind('/').map_or("", |i| &path[..i])
}

/// A path with `.` and `..` segments folded (Python's `posixpath.normpath`,
/// minus the leading-slash handling: results here are always relative).
fn normalize_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// Resolve `href` against the document at `base`: an absolute URL stands, a
/// relative one is taken from the base document's directory — inside its URL
/// when that is remote, else as a path under the taxonomy directory (the
/// instance itself sits at its root, base `""`).
pub(crate) fn join_url(base: &str, href: &str) -> String {
    if is_remote(href) {
        return href.to_string();
    }
    if let Some((scheme, rest)) = base.split_once("://") {
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        let joined = normalize_path(&format!("{}/{href}", dirname(path)));
        return format!("{scheme}://{host}/{joined}");
    }
    normalize_path(&format!("{}/{href}", dirname(base)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #1192: a taxonomy reference reads a file inside the taxonomy
    /// directory or nothing — a drive, UNC or rooted path, a `..` with
    /// either separator, percent-encoded dots (a name like any other, absent
    /// here) and a symlink out of the directory all resolve to nothing, on
    /// every platform alike.
    #[test]
    fn taxonomy_reads_stay_inside_the_directory() {
        let root = std::env::temp_dir().join(format!("xbrl-confine-{}", std::process::id()));
        let dir = root.join("taxonomy");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/x.xsd"), b"<schema/>").unwrap();
        std::fs::write(root.join("x.xsd"), b"outside").unwrap();
        assert!(confined(&dir, "sub/x.xsd").is_some());
        assert!(confined(&dir, "sub\\x.xsd").is_some());
        for url in [
            "C:/x.xsd",
            "C:\\x.xsd",
            "c:x.xsd",
            "\\\\server\\share\\x.xsd",
            "//server/share/x.xsd",
            "/x.xsd",
            "\\x.xsd",
            "../x.xsd",
            "..\\x.xsd",
            "..\\..\\x.xsd",
            "sub/../../x.xsd",
            "sub\\..\\..\\x.xsd",
            "%2e%2e/x.xsd",
        ] {
            assert!(confined(&dir, url).is_none(), "{url}");
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&root, dir.join("up")).unwrap();
            assert!(confined(&dir, "up/x.xsd").is_none(), "a symlink out");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn urls_join_like_arelle_resolves_them() {
        assert_eq!(join_url("", "acme-2025.xsd"), "acme-2025.xsd");
        assert_eq!(
            join_url("acme-2025.xsd", "acme-2025_pre.xml"),
            "acme-2025_pre.xml"
        );
        assert_eq!(join_url("tax/acme.xsd", "../shared/x.xml"), "shared/x.xml");
        assert_eq!(
            join_url(
                "https://xbrl.sec.gov/dei/2025/dei-sub-2025.xsd",
                "dei-2025_pre.xsd"
            ),
            "https://xbrl.sec.gov/dei/2025/dei-2025_pre.xsd"
        );
        assert_eq!(
            join_url(
                "acme.xsd",
                "https://xbrl.fasb.org/us-gaap/2025/elts/us-gaap-2025.xsd"
            ),
            "https://xbrl.fasb.org/us-gaap/2025/elts/us-gaap-2025.xsd"
        );
        assert_eq!(
            normalize_path("taxonomy_package/META-INF/../https/x"),
            "taxonomy_package/https/x"
        );
    }

    #[test]
    fn effective_arcs_resolve_priority_and_prohibition_then_sort_by_order() {
        let arc = |from: &str, to: &str, order: f64, priority: i64, prohibited: bool| Arc {
            role: "r".into(),
            from: from.into(),
            to: to.into(),
            order,
            priority,
            prohibited,
            weight: None,
        };
        let arcs = vec![
            arc("A", "b", 2.0, 0, false),
            arc("A", "c", 1.0, 0, false),
            // A later prohibiting arc of equal priority removes the first.
            arc("A", "b", 2.0, 0, true),
            arc("A", "d", 1.0, 0, false),
            // A prohibition outranked by priority does nothing.
            arc("A", "c", 1.0, -1, true),
        ];
        let kept: Vec<&str> = effective(&arcs).iter().map(|a| a.to.as_str()).collect();
        assert_eq!(kept, ["c", "d"]);
    }
}
