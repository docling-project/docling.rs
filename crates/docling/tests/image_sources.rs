//! The image-source policy (#646): which `<img src>` / `![…](…)` / `cid:`
//! references the declarative backends resolve, end to end through
//! [`DocumentConverter`]. The resolver's own rules — path confinement, the
//! host allow-list on redirects, the budget — are unit-tested in
//! `backend/images.rs`; this file is the issue's acceptance list on real
//! documents: HTML under each tier, Markdown `data:` images, an `.eml` and a
//! `.msg` with two inline pictures, the per-document limit, and the
//! `fetch_images` alias.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use docling::{DocumentConverter, ImageSources, SourceDocument};
use docling_core::Node;

fn fixture(format: &str, name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(format)
        .join("sources")
        .join(name)
}

/// A solid-colour PNG of `w`×`h` pixels, encoded in memory.
fn png(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
    let img = image::RgbImage::from_pixel(w, h, image::Rgb(rgb));
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

fn data_uri(bytes: &[u8]) -> String {
    format!("data:image/png;base64,{}", docling::base64::encode(bytes))
}

/// `(has image bytes, (width, height))` per flat picture node, in order.
fn pictures(doc: &docling_core::DoclingDocument) -> Vec<Option<(u32, u32)>> {
    doc.nodes
        .iter()
        .filter_map(|n| match n {
            Node::Picture { image, .. } => Some(image.as_ref().map(|i| (i.width, i.height))),
            _ => None,
        })
        .collect()
}

/// The JSON export's pictures: `(has image, alt/caption text)`.
fn json_pictures(doc: &docling_core::DoclingDocument) -> Vec<(bool, Option<(u64, u64)>)> {
    let json: serde_json::Value = serde_json::from_str(&doc.export_to_json()).unwrap();
    json["pictures"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            let size = p["image"]["size"].as_object().map(|s| {
                (
                    s["width"].as_f64().unwrap() as u64,
                    s["height"].as_f64().unwrap() as u64,
                )
            });
            (p.get("image").is_some(), size)
        })
        .collect()
}

fn html_source(dir: &Path, name: &str, html: &str) -> SourceDocument {
    let path = dir.join(name);
    std::fs::write(&path, html).unwrap();
    SourceDocument::from_file(&path).unwrap()
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("docling-646-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A loopback HTTP server that serves a PNG to every request and counts
/// the requests it saw — the "no outbound connection" spy.
fn counting_server() -> (String, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let server_hits = Arc::clone(&hits);
    let body = png(1, 1, [0, 0, 0]);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            server_hits.fetch_add(1, Ordering::Relaxed);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    (format!("http://127.0.0.1:{}", addr.port()), hits)
}

/// Acceptance 1: under `Embedded` only the `data:` image resolves. The URL
/// points at a counting server that sees no request, the absolute path
/// names a real file that is not read.
#[test]
fn html_embedded_resolves_data_uris_only() {
    let dir = temp_dir("embedded");
    let (base, hits) = counting_server();
    let real_file = dir.join("secret.png");
    std::fs::write(&real_file, png(3, 3, [9, 9, 9])).unwrap();
    let html = format!(
        r#"<html><body>
<img src="{}" alt="inline">
<img src="{base}/remote.png" alt="remote">
<img src="{}" alt="absolute">
<img src="/etc/hosts" alt="etc">
</body></html>"#,
        data_uri(&png(4, 2, [255, 0, 0])),
        real_file.display()
    );
    let source = html_source(&dir, "page.html", &html);
    let doc = DocumentConverter::new()
        .image_sources(ImageSources::Embedded)
        .convert(source)
        .unwrap()
        .document;
    assert_eq!(pictures(&doc), [Some((4, 2)), None, None, None]);
    assert_eq!(hits.load(Ordering::Relaxed), 0, "no outbound connection");
    // The JSON tree carries the same single image.
    let json = json_pictures(&doc);
    assert_eq!(json.len(), 4);
    assert_eq!(json[0], (true, Some((4, 2))));
    assert!(json[1..].iter().all(|(has, _)| !has));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Acceptance 2: under `Local` a file under the source's directory
/// resolves, `../outside.png` is refused.
#[test]
fn html_local_is_confined_to_the_source_directory() {
    let root = temp_dir("local");
    let base = root.join("doc");
    std::fs::create_dir_all(base.join("img")).unwrap();
    std::fs::write(base.join("img/inside.png"), png(5, 7, [0, 255, 0])).unwrap();
    std::fs::write(root.join("outside.png"), png(2, 2, [0, 0, 255])).unwrap();
    let html = r#"<html><body>
<img src="img/inside.png" alt="in">
<img src="../outside.png" alt="out">
</body></html>"#;
    let source = html_source(&base, "page.html", html);
    let doc = DocumentConverter::new()
        .image_sources(ImageSources::Local)
        .convert(source)
        .unwrap()
        .document;
    assert_eq!(pictures(&doc), [Some((5, 7)), None]);
    // `Embedded` reads no file at all.
    let source = html_source(&base, "page.html", html);
    let doc = DocumentConverter::new()
        .image_sources(ImageSources::Embedded)
        .convert(source)
        .unwrap()
        .document;
    assert_eq!(pictures(&doc), [None, None]);
    let _ = std::fs::remove_dir_all(&root);
}

/// #1030: a bare relative file name (`cd dir && docling-rs page.html`)
/// resolves its local images against the working directory. Its
/// `Path::parent` is the empty path, which used to fail `canonicalize`, so
/// nothing resolved while `./page.html` did. Tests run with the crate dir as
/// CWD, so a crate fixture stands in for the image next to the page.
#[test]
fn html_local_resolves_for_a_bare_relative_file_name() {
    let html =
        r#"<html><body><img src="tests/fixtures/textline/axis_title.png" alt="x"></body></html>"#;
    for name in ["page.html", "./page.html"] {
        let mut source =
            SourceDocument::from_bytes(name, docling::InputFormat::Html, html.as_bytes().to_vec());
        source.path = Some(PathBuf::from(name));
        assert_eq!(
            source.base_dir().map(Path::to_path_buf),
            Some(PathBuf::from(".")),
            "{name}"
        );
        let doc = DocumentConverter::new()
            .image_sources(ImageSources::Local)
            .convert(source)
            .unwrap()
            .document;
        let pics = pictures(&doc);
        assert_eq!(pics.len(), 1, "{name}");
        assert!(pics[0].is_some(), "{name}: the local image did not resolve");
    }
}

/// Acceptance 3: under `Remote` with `image_hosts`, a host off the list is
/// never contacted; the same URL resolves with the host listed (the
/// redirect rule is proven in the resolver's unit test).
#[test]
fn html_remote_honours_the_host_allow_list() {
    let dir = temp_dir("hosts");
    let (base, hits) = counting_server();
    // 127.0.0.1 is on the SSRF block-list; the resolver's own tests opt in
    // the same way for a loopback server.
    std::env::set_var("DOCLING_RS_ALLOW_PRIVATE_IP_FETCH", "1");
    let html = format!(r#"<html><body><img src="{base}/pic.png" alt="r"></body></html>"#);
    let source = html_source(&dir, "page.html", &html);
    let doc = DocumentConverter::new()
        .image_sources(ImageSources::Remote)
        .image_hosts(["cdn.example.com"])
        .convert(source)
        .unwrap()
        .document;
    assert_eq!(pictures(&doc), [None]);
    assert_eq!(
        hits.load(Ordering::Relaxed),
        0,
        "a host off the list is never contacted"
    );
    let source = html_source(&dir, "page.html", &html);
    let doc = DocumentConverter::new()
        .image_sources(ImageSources::Remote)
        .image_hosts(["127.0.0.1"])
        .convert(source)
        .unwrap()
        .document;
    assert_eq!(pictures(&doc), [Some((1, 1))]);
    assert_eq!(hits.load(Ordering::Relaxed), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Acceptance 4: a Markdown `![…](data:…)` image and an inline
/// `<img src="data:…">` in an HTML paragraph become pictures with bytes
/// under `Embedded`; the relative path stays a placeholder; under `None`
/// the pictures carry nothing and print as placeholders. A lone image is
/// captioned by its title, its alt text a paragraph after it (#760,
/// docling#4777).
#[test]
fn markdown_data_images_resolve_under_embedded() {
    let source = SourceDocument::from_file(fixture("md", "data_uri_images.md")).unwrap();
    let doc = DocumentConverter::new()
        .image_sources(ImageSources::Embedded)
        .convert(source)
        .unwrap()
        .document;
    // The lone-image paragraph is a picture in the flat chain; the inline
    // `<img>` inside `<p>` comes through the HTML fragment as a picture too.
    assert_eq!(pictures(&doc), [Some((24, 16)), Some((16, 24))]);
    assert!(matches!(
        &doc.nodes[2],
        Node::Picture { caption: Some(c), .. } if c == "Red box"
    ));
    assert!(matches!(&doc.nodes[3], Node::Paragraph { text } if text == "A red box"));
    let json = json_pictures(&doc);
    assert_eq!(json.iter().filter(|(has, _)| *has).count(), 2, "{json:?}");
    assert!(json.iter().any(|(_, s)| *s == Some((24, 16))));

    let source = SourceDocument::from_file(fixture("md", "data_uri_images.md")).unwrap();
    let plain = DocumentConverter::new().convert(source).unwrap().document;
    assert!(json_pictures(&plain).iter().all(|(has, _)| !has));
    assert!(pictures(&plain).iter().all(Option::is_none));
    let md = plain.export_to_markdown();
    assert!(
        md.contains("Red box\n\n<!-- image -->\n\nA red box"),
        "{md}"
    );
    assert!(!md.contains("data:image/png;base64,"), "{md}");
}

/// Acceptance 5: an `.eml` whose HTML body references two `cid:` parts
/// yields both pictures, in body order, with the text around them; the
/// default tier keeps docling's paragraph body without pictures.
#[test]
fn eml_cid_images_resolve_in_body_order() {
    let path = fixture("email", "eml_inline_images.eml");
    let doc = DocumentConverter::new()
        .image_sources(ImageSources::Embedded)
        .convert(SourceDocument::from_file(&path).unwrap())
        .unwrap()
        .document;
    assert_eq!(pictures(&doc), [Some((24, 16)), Some((16, 24))]);
    let texts: Vec<&str> = doc
        .nodes
        .iter()
        .filter_map(|n| match n {
            Node::Heading { text, .. } | Node::Paragraph { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts[0], "Two inline pictures");
    assert!(texts.contains(&"Report"), "{texts:?}");
    let red = doc
        .nodes
        .iter()
        .position(|n| matches!(n, Node::Picture { image: Some(i), .. } if i.width == 24))
        .unwrap();
    let blue = doc
        .nodes
        .iter()
        .position(|n| matches!(n, Node::Picture { image: Some(i), .. } if i.width == 16))
        .unwrap();
    let first = doc
        .nodes
        .iter()
        .position(|n| matches!(n, Node::Paragraph { text } if text.contains("First the red")))
        .unwrap();
    let then = doc
        .nodes
        .iter()
        .position(|n| matches!(n, Node::Paragraph { text } if text.contains("Then the blue")))
        .unwrap();
    assert!(first < red && red < then && then < blue, "body order kept");
    assert_eq!(json_pictures(&doc).len(), 2);

    let plain = DocumentConverter::new()
        .convert(SourceDocument::from_file(&path).unwrap())
        .unwrap()
        .document;
    assert!(pictures(&plain).is_empty());
    assert!(plain
        .nodes
        .iter()
        .any(|n| matches!(n, Node::Paragraph { text } if text.contains("First the red box"))));
}

/// Acceptance 5, `.msg`: the HTML body (PR_HTML) with its
/// PR_ATTACH_CONTENT_ID attachments resolves the same way; the default
/// tier keeps the plain-text body.
#[test]
fn msg_cid_images_resolve_in_body_order() {
    let path = fixture("email", "msg_inline_images.msg");
    let doc = DocumentConverter::new()
        .image_sources(ImageSources::Embedded)
        .convert(SourceDocument::from_file(&path).unwrap())
        .unwrap()
        .document;
    assert_eq!(pictures(&doc), [Some((24, 16)), Some((16, 24))]);
    assert!(
        matches!(&doc.nodes[0], Node::Heading { text, .. } if text == "Two inline pictures in a msg")
    );
    assert!(doc
        .nodes
        .iter()
        .any(|n| matches!(n, Node::Heading { text, .. } if text == "Quarterly figures")));

    let plain = DocumentConverter::new()
        .convert(SourceDocument::from_file(&path).unwrap())
        .unwrap()
        .document;
    assert!(pictures(&plain).is_empty());
    assert!(plain
        .nodes
        .iter()
        .any(|n| matches!(n, Node::Paragraph { text } if text == "Red first:")));
}

/// Acceptance 6: `max_images=1` on a page with three images keeps one and
/// leaves two placeholders; the conversion succeeds.
#[test]
fn max_images_keeps_the_first_and_leaves_placeholders() {
    let dir = temp_dir("limits");
    let html = format!(
        r#"<html><body><img src="{}" alt="a"><img src="{}" alt="b"><img src="{}" alt="c"></body></html>"#,
        data_uri(&png(2, 2, [1, 1, 1])),
        data_uri(&png(3, 3, [2, 2, 2])),
        data_uri(&png(4, 4, [3, 3, 3]))
    );
    let source = html_source(&dir, "page.html", &html);
    let result = DocumentConverter::new()
        .image_sources(ImageSources::Embedded)
        .max_images(1)
        .convert(source)
        .unwrap();
    assert_eq!(pictures(&result.document), [Some((2, 2)), None, None]);
    // A budget is per document: a second conversion starts afresh.
    let source = html_source(&dir, "page.html", &html);
    let again = DocumentConverter::new()
        .image_sources(ImageSources::Embedded)
        .max_images(1)
        .convert(source)
        .unwrap();
    assert_eq!(pictures(&again.document), [Some((2, 2)), None, None]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Acceptance 7: the `fetch_images` alias maps onto the tiers.
#[test]
fn fetch_images_is_the_remote_alias() {
    assert_eq!(
        DocumentConverter::new()
            .fetch_images(true)
            .image_policy()
            .sources,
        ImageSources::Remote
    );
    assert_eq!(
        DocumentConverter::new()
            .fetch_images(false)
            .image_policy()
            .sources,
        ImageSources::None
    );
    assert_eq!(
        DocumentConverter::new().image_policy().sources,
        ImageSources::None
    );
    // The wire form: `image_sources` wins over the alias, hosts split on
    // commas, limits land.
    let o: docling::ConvertOptions = serde_json::from_str(
        r#"{"fetch_images": true, "image_sources": "embedded", "image_hosts": "a.example, *.b.example", "max_images": 2}"#,
    )
    .unwrap();
    let c = o.apply(DocumentConverter::new()).unwrap();
    let p = c.image_policy();
    assert_eq!(p.sources, ImageSources::Embedded);
    assert_eq!(p.hosts, ["a.example", "*.b.example"]);
    assert_eq!(p.limits.max_images, Some(2));
    let bad: docling::ConvertOptions =
        serde_json::from_str(r#"{"image_sources": "everything"}"#).unwrap();
    assert_eq!(bad.validate().unwrap_err().field, "image_sources");
}
