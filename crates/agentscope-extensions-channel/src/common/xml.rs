//! Minimal XML text extraction for the WeCom callback parser.
//!
//! The Java mapper uses a hardened `DocumentBuilderFactory` + `getElementsByTagName(tag)` +
//! `getTextContent()`. [`xml_element_text`] reproduces that behaviour with `quick-xml`: it returns
//! the concatenated text (including CDATA) of the **first** element with the given local name at
//! any depth, or `None` when the element is absent.
//!
//! Malformed input never panics: parse errors surface as `None`, which the callers treat as an
//! unparseable payload.

use quick_xml::escape::{resolve_predefined_entity, unescape};
use quick_xml::events::Event;
use quick_xml::Reader;

/// Extract the text content of the first `tag` element in `xml`.
pub fn xml_element_text(xml: &str, tag: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    let mut capture = false;
    let mut depth: usize = 0;
    let mut out = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = local_name(e.local_name().as_ref());
                if capture {
                    depth += 1;
                } else if name == tag {
                    capture = true;
                    depth = 1;
                }
            }
            Ok(Event::Empty(e)) => {
                // `<Tag/>` has no text content; DOM `getTextContent()` returns "".
                if !capture {
                    let name = local_name(e.local_name().as_ref());
                    if name == tag {
                        return Some(String::new());
                    }
                }
            }
            Ok(Event::End(_)) => {
                if capture {
                    depth -= 1;
                    if depth == 0 {
                        return Some(out);
                    }
                }
            }
            Ok(Event::Text(e)) => {
                if capture {
                    if let Ok(decoded) = e.decode() {
                        if let Ok(unescaped) = unescape(&decoded) {
                            out.push_str(&unescaped);
                        }
                    }
                }
            }
            Ok(Event::GeneralRef(e)) => {
                // `quick-xml` splits `&amp;` out of `Text` into a `GeneralRef` event; DOM
                // `getTextContent()` resolves it in place.
                if capture {
                    match e.decode().ok().and_then(|name| resolve_general_ref(&name)) {
                        Some(text) => out.push_str(&text),
                        // An undeclared entity makes the DOM parse fail -> unusable payload.
                        None => return None,
                    }
                }
            }
            Ok(Event::CData(e)) => {
                if capture {
                    if let Ok(decoded) = e.decode() {
                        out.push_str(&decoded);
                    }
                }
            }
            Ok(Event::Eof) => break,
            // Any malformed construct (bad entity, unclosed tag, disallowed DOCTYPE, ...) makes
            // the payload unusable, matching the Java mapper's exception path.
            Err(_) => return None,
            _ => {}
        }
    }
    None
}

/// Resolve a `&name;` / `&#NN;` / `&#xHH;` general reference the way an XML processor does.
fn resolve_general_ref(name: &str) -> Option<String> {
    if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
        return char::from_u32(u32::from_str_radix(hex, 16).ok()?).map(|c| c.to_string());
    }
    if let Some(dec) = name.strip_prefix('#') {
        return char::from_u32(dec.parse().ok()?).map(|c| c.to_string());
    }
    resolve_predefined_entity(name).map(|s| s.to_string())
}

fn local_name(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const WECOM: &str = "<xml><ToUserName>corp</ToUserName><FromUserName>user-1</FromUserName>\
<CreateTime>1348831860</CreateTime><MsgType>text</MsgType>\
<Content>hello &amp; goodbye</Content><MsgId>1234567890123456</MsgId><AgentID>1</AgentID></xml>";

    #[test]
    fn extracts_plain_text_fields() {
        assert_eq!(xml_element_text(WECOM, "MsgType").as_deref(), Some("text"));
        assert_eq!(
            xml_element_text(WECOM, "FromUserName").as_deref(),
            Some("user-1")
        );
        assert_eq!(
            xml_element_text(WECOM, "MsgId").as_deref(),
            Some("1234567890123456")
        );
    }

    #[test]
    fn unescapes_entities_in_text() {
        assert_eq!(
            xml_element_text(WECOM, "Content").as_deref(),
            Some("hello & goodbye")
        );
    }

    #[test]
    fn reads_cdata_content_like_dom_get_text_content() {
        let xml = "<xml><Content><![CDATA[a < b & c]]></Content></xml>";
        assert_eq!(
            xml_element_text(xml, "Content").as_deref(),
            Some("a < b & c")
        );
    }

    #[test]
    fn preserves_inner_elements_text() {
        let xml = "<xml><Outer><Inner>deep</Inner></Outer></xml>";
        assert_eq!(xml_element_text(xml, "Inner").as_deref(), Some("deep"));
        assert_eq!(xml_element_text(xml, "Outer").as_deref(), Some("deep"));
    }

    #[test]
    fn returns_none_for_missing_tag_and_empty_element() {
        assert!(xml_element_text(WECOM, "Nope").is_none());
        assert_eq!(
            xml_element_text("<xml><A/></xml>", "A").as_deref(),
            Some("")
        );
    }

    #[test]
    fn returns_none_for_malformed_xml() {
        assert!(xml_element_text("<xml><A>oops</xml>", "A").is_none());
        assert!(xml_element_text("not xml at all", "A").is_none());
    }
}
