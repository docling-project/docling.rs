//! Throwaway: dump a page's operator histogram, fonts, and XObject subtypes.
//! Usage: probe_page <pdf> <page_index>
use std::collections::BTreeMap;

use lopdf::{Document, Object};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = &args[1];
    let idx: usize = args[2].parse().unwrap();
    let doc = Document::load(path).unwrap();
    let mut pages: Vec<_> = doc.get_pages().into_iter().collect();
    pages.sort_by_key(|(n, _)| *n);
    let (_, pid) = pages[idx];

    let fonts = doc.get_page_fonts(pid).unwrap_or_default();
    println!("fonts:");
    for (name, d) in &fonts {
        let st = d
            .get(b"Subtype")
            .ok()
            .and_then(|o| o.as_name().ok())
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .unwrap_or_default();
        let bf = d
            .get(b"BaseFont")
            .ok()
            .and_then(|o| o.as_name().ok())
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .unwrap_or_default();
        println!("  {} {} {}", String::from_utf8_lossy(name), st, bf);
    }

    let (res, ids) = doc.get_page_resources(pid).unwrap();
    let resd = res.cloned().or_else(|| {
        ids.into_iter()
            .find_map(|id| doc.get_dictionary(id).ok().cloned())
    });
    if let Some(rd) = &resd {
        if let Some(Object::Dictionary(xo)) = rd.get(b"XObject").ok().map(|o| match o {
            Object::Reference(r) => doc.get_object(*r).unwrap().clone(),
            other => other.clone(),
        }) {
            println!("XObjects:");
            for (name, v) in xo.iter() {
                let st = match v {
                    Object::Reference(r) => doc
                        .get_object(*r)
                        .ok()
                        .and_then(|o| o.as_stream().ok())
                        .and_then(|s| s.dict.get(b"Subtype").ok().and_then(|o| o.as_name().ok()))
                        .map(|n| String::from_utf8_lossy(n).into_owned()),
                    _ => None,
                };
                println!("  {} {:?}", String::from_utf8_lossy(name), st);
            }
        }
    }

    let content_bytes = doc.get_page_content(pid).unwrap();
    let content = lopdf::content::Content::decode(&content_bytes).unwrap();
    let mut hist: BTreeMap<String, usize> = BTreeMap::new();
    for op in &content.operations {
        *hist.entry(op.operator.clone()).or_default() += 1;
    }
    println!("operators: {:?}", hist);
}
