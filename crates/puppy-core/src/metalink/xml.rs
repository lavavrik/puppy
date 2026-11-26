//! A small, bounded element tree over `quick-xml`, enough for Metalink.
//!
//! What makes it safe for documents from the network:
//! - a `<!DOCTYPE …>` is refused outright: no DTD is ever read, so there are no entity
//!   definitions to expand (billion laughs) and nothing external to fetch (XXE);
//! - only XML's five predefined entities and character references are resolved; any
//!   other `&name;` is an error;
//! - nesting depth and the number of elements are capped (the document's size is capped
//!   by the caller before parsing).

use super::MetalinkError;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::{NsReader, XmlVersion};

/// Deeper nesting is refused (Metalink needs four levels).
const MAX_DEPTH: usize = 32;
/// More elements are refused (a thousand files with a hundred URLs each fit easily).
const MAX_ELEMENTS: usize = 250_000;

/// One element: its namespace (`None` when unbound), local name, attributes (local
/// names; namespace declarations left out), concatenated text and child elements.
#[derive(Debug, Default)]
pub(super) struct Element {
    pub ns: Option<String>,
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub children: Vec<Element>,
}

impl Element {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The children called `name` in namespace `ns`.
    pub fn children<'a>(
        &'a self,
        ns: Option<&'a str>,
        name: &'a str,
    ) -> impl Iterator<Item = &'a Element> + 'a {
        self.children
            .iter()
            .filter(move |c| c.ns.as_deref() == ns && c.name == name)
    }

    pub fn child<'a>(&'a self, ns: Option<&'a str>, name: &'a str) -> Option<&'a Element> {
        self.children(ns, name).next()
    }

    /// The text, trimmed.
    pub fn value(&self) -> &str {
        self.text.trim()
    }
}

fn bad(e: impl std::fmt::Display) -> MetalinkError {
    MetalinkError::Xml(e.to_string())
}

fn open(ns: ResolveResult<'_>, e: &BytesStart<'_>) -> Result<Element, MetalinkError> {
    let ns = match ns {
        ResolveResult::Bound(n) => Some(String::from_utf8_lossy(n.as_ref()).into_owned()),
        ResolveResult::Unbound => None,
        ResolveResult::Unknown(p) => {
            return Err(bad(format_args!(
                "unknown namespace prefix {:?}",
                String::from_utf8_lossy(&p)
            )));
        }
    };
    let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
    let mut attrs = Vec::new();
    for a in e.attributes() {
        let a = a.map_err(bad)?;
        if a.key.as_namespace_binding().is_some() {
            continue;
        }
        let key = String::from_utf8_lossy(a.key.local_name().as_ref()).into_owned();
        let value = a.normalized_value(XmlVersion::Implicit1_0).map_err(bad)?;
        attrs.push((key, value.into_owned()));
    }
    Ok(Element {
        ns,
        name,
        attrs,
        ..Element::default()
    })
}

/// Parses `text` into its root element.
pub(super) fn parse(text: &str) -> Result<Element, MetalinkError> {
    let mut reader = NsReader::from_str(text);
    let mut stack: Vec<Element> = Vec::new();
    let mut root = None;
    let mut count = 0usize;
    loop {
        let (ns, event) = reader.read_resolved_event().map_err(bad)?;
        let finished = match event {
            Event::DocType(_) => return Err(MetalinkError::Doctype),
            Event::Start(ref e) | Event::Empty(ref e) => {
                if root.is_some() {
                    return Err(bad("content after the root element"));
                }
                count += 1;
                if count > MAX_ELEMENTS {
                    return Err(MetalinkError::TooComplex(format!(
                        "more than {MAX_ELEMENTS} elements"
                    )));
                }
                if stack.len() >= MAX_DEPTH {
                    return Err(MetalinkError::TooComplex(format!(
                        "elements nested deeper than {MAX_DEPTH}"
                    )));
                }
                let el = open(ns, e)?;
                if matches!(event, Event::Start(_)) {
                    stack.push(el);
                    None
                } else {
                    Some(el)
                }
            }
            Event::End(_) => stack.pop(),
            Event::Text(t) => {
                let t = t.xml10_content().map_err(bad)?;
                match stack.last_mut() {
                    Some(top) => top.text.push_str(&t),
                    None if t.trim().is_empty() => {}
                    None => return Err(bad("text outside the root element")),
                }
                None
            }
            Event::CData(t) => {
                let t = t.xml10_content().map_err(bad)?;
                match stack.last_mut() {
                    Some(top) => top.text.push_str(&t),
                    None => return Err(bad("CDATA outside the root element")),
                }
                None
            }
            Event::GeneralRef(r) => {
                let name = r.decode().map_err(bad)?;
                let resolved = match r.resolve_char_ref().map_err(bad)? {
                    Some(c) => c.to_string(),
                    None => match quick_xml::escape::resolve_predefined_entity(&name) {
                        Some(s) => s.to_string(),
                        None => {
                            return Err(bad(format_args!(
                                "unknown entity &{name}; (only XML's predefined ones are \
                                 allowed)"
                            )));
                        }
                    },
                };
                match stack.last_mut() {
                    Some(top) => top.text.push_str(&resolved),
                    None => return Err(bad("a reference outside the root element")),
                }
                None
            }
            Event::Decl(_) | Event::PI(_) | Event::Comment(_) => None,
            Event::Eof => break,
        };
        if let Some(el) = finished {
            match stack.last_mut() {
                Some(parent) => parent.children.push(el),
                None => root = Some(el),
            }
        }
    }
    if !stack.is_empty() {
        return Err(bad("the document ends inside an element"));
    }
    root.ok_or_else(|| bad("no root element"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_with_namespaces_entities_and_cdata() {
        let doc = r#"<?xml version="1.0"?>
            <!-- a comment -->
            <a xmlns="urn:x" xmlns:o="urn:o" k="1 &amp; 2">
              <b>x &lt; y &#x41;<![CDATA[<raw>]]></b>
              <o:b/>
            </a>"#;
        let root = parse(doc).unwrap();
        assert_eq!(root.ns.as_deref(), Some("urn:x"));
        assert_eq!(root.attr("k"), Some("1 & 2"));
        assert_eq!(
            root.attrs.len(),
            1,
            "namespace declarations are not attributes"
        );
        let b: Vec<_> = root.children(Some("urn:x"), "b").collect();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].value(), "x < y A<raw>");
        assert!(root.child(Some("urn:o"), "b").is_some());
    }

    #[test]
    fn refuses_dtds_unknown_entities_and_junk() {
        let doctype = r#"<?xml version="1.0"?>
            <!DOCTYPE lolz [<!ENTITY lol "lol"><!ENTITY lol2 "&lol;&lol;&lol;">]>
            <metalink>&lol2;</metalink>"#;
        assert_eq!(parse(doctype).unwrap_err(), MetalinkError::Doctype);
        let external = r#"<!DOCTYPE m SYSTEM "file:///etc/passwd"><m/>"#;
        assert_eq!(parse(external).unwrap_err(), MetalinkError::Doctype);
        let unknown = "<m>&xxe;</m>";
        assert!(
            matches!(parse(unknown), Err(MetalinkError::Xml(m)) if m.contains("&xxe;")),
            "{:?}",
            parse(unknown)
        );
        for junk in [
            "", "<a>", "<a></b>", "<a/><b/>", "text<a/>", "<a/>tail", "<p:a/>",
        ] {
            assert!(
                matches!(parse(junk), Err(MetalinkError::Xml(_))),
                "{junk:?}: {:?}",
                parse(junk)
            );
        }
    }

    #[test]
    fn depth_and_element_count_are_capped() {
        let deep = "<a>".repeat(MAX_DEPTH + 1) + &"</a>".repeat(MAX_DEPTH + 1);
        assert!(matches!(parse(&deep), Err(MetalinkError::TooComplex(_))));
        let ok = "<a>".repeat(MAX_DEPTH) + &"</a>".repeat(MAX_DEPTH);
        assert!(parse(&ok).is_ok());
        let wide = format!("<a>{}</a>", "<b/>".repeat(MAX_ELEMENTS));
        assert!(matches!(parse(&wide), Err(MetalinkError::TooComplex(_))));
    }
}
