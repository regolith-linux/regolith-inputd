//! A small XML reader for IBus component descriptions and the engine lists
//! their commands print.
//!
//! It checks that a document is well-formed (one root element, matching
//! tags, quoted attributes, known references, nothing but comments,
//! processing instructions and blanks around the root) and gives the
//! elements as a tree. There is no DTD support beyond skipping a document
//! type declaration, and no namespace handling.

use std::fmt;

/// An element: its name, its attributes in document order, and its children.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Element {
    pub name: String,
    pub attributes: Vec<(String, String)>,
    pub children: Vec<Node>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Node {
    Element(Element),
    Text(String),
}

impl Element {
    /// The value of the attribute `name`.
    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The child elements, in document order.
    pub fn elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|node| match node {
            Node::Element(element) => Some(element),
            Node::Text(_) => None,
        })
    }

    /// The child elements named `name`.
    pub fn elements_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> {
        self.elements().filter(move |e| e.name == name)
    }

    /// The text directly inside the element, without leading or trailing
    /// blanks.
    pub fn text(&self) -> String {
        let mut text = String::new();
        for node in &self.children {
            if let Node::Text(t) = node {
                text.push_str(t);
            }
        }
        text.trim().to_string()
    }

    /// The text of the first child element named `name`.
    pub fn child_text(&self, name: &str) -> Option<String> {
        self.elements_named(name).next().map(Element::text)
    }
}

/// Why a document isn't well-formed, and where.
#[derive(Debug, PartialEq, Eq)]
pub struct Error {
    pub offset: usize,
    pub message: &'static str,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte {}", self.message, self.offset)
    }
}

impl std::error::Error for Error {}

/// Read a whole document and give its root element.
pub fn parse(document: &str) -> Result<Element, Error> {
    let document = document.strip_prefix('\u{feff}').unwrap_or(document);
    // XML reads every line ending as a line feed.
    let normalized;
    let document = if document.contains('\r') {
        normalized = document.replace("\r\n", "\n").replace('\r', "\n");
        normalized.as_str()
    } else {
        document
    };
    let mut reader = Reader {
        text: document,
        pos: 0,
    };
    reader.misc(true)?;
    if !reader.text[reader.pos..].starts_with('<') {
        return Err(reader.error("the document has no root element"));
    }
    let root = reader.element()?;
    reader.misc(false)?;
    if reader.pos != reader.text.len() {
        return Err(reader.error("content after the root element"));
    }
    Ok(root)
}

struct Reader<'a> {
    text: &'a str,
    pos: usize,
}

fn is_blank(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == ':' || !c.is_ascii()
}

fn is_name_char(c: char) -> bool {
    is_name_start(c) || c.is_ascii_digit() || c == '-' || c == '.'
}

impl<'a> Reader<'a> {
    fn error(&self, message: &'static str) -> Error {
        Error {
            offset: self.pos,
            message,
        }
    }

    fn rest(&self) -> &'a str {
        &self.text[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn eat(&mut self, s: &str) -> bool {
        if self.rest().starts_with(s) {
            self.pos += s.len();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, s: &str, message: &'static str) -> Result<(), Error> {
        if self.eat(s) {
            Ok(())
        } else {
            Err(self.error(message))
        }
    }

    fn skip_blanks(&mut self) -> bool {
        let start = self.pos;
        while let Some(c) = self.peek().filter(|c| is_blank(*c)) {
            self.pos += c.len_utf8();
        }
        self.pos > start
    }

    /// Skip past the next `end`, giving what came before it.
    fn until(&mut self, end: &str, message: &'static str) -> Result<&'a str, Error> {
        match self.rest().find(end) {
            Some(i) => {
                let skipped = &self.rest()[..i];
                self.pos += i + end.len();
                Ok(skipped)
            }
            None => Err(self.error(message)),
        }
    }

    /// Comments, processing instructions and blanks outside the root
    /// element; before it, also the XML and document type declarations.
    fn misc(&mut self, prolog: bool) -> Result<(), Error> {
        loop {
            self.skip_blanks();
            if self.eat("<!--") {
                self.until("-->", "unterminated comment")?;
            } else if self.eat("<?") {
                self.until("?>", "unterminated processing instruction")?;
            } else if prolog && self.rest().starts_with("<!DOCTYPE") {
                self.doctype()?;
            } else {
                return Ok(());
            }
        }
    }

    fn doctype(&mut self) -> Result<(), Error> {
        let mut depth = 0;
        let mut quote = None;
        for (i, c) in self.rest().char_indices() {
            match (quote, c) {
                (Some(q), c) if c == q => quote = None,
                (Some(_), _) => (),
                (None, '"' | '\'') => quote = Some(c),
                (None, '[') => depth += 1,
                (None, ']') => depth -= 1,
                (None, '>') if depth == 0 => {
                    self.pos += i + 1;
                    return Ok(());
                }
                _ => (),
            }
        }
        Err(self.error("unterminated document type declaration"))
    }

    fn name(&mut self) -> Result<String, Error> {
        let start = self.pos;
        match self.peek() {
            Some(c) if is_name_start(c) => self.pos += c.len_utf8(),
            _ => return Err(self.error("a name was expected")),
        }
        while let Some(c) = self.peek().filter(|c| is_name_char(*c)) {
            self.pos += c.len_utf8();
        }
        Ok(self.text[start..self.pos].to_string())
    }

    /// An element, from its `<` to the end of its end tag.
    fn element(&mut self) -> Result<Element, Error> {
        self.expect("<", "an element was expected")?;
        let mut element = Element {
            name: self.name()?,
            ..Element::default()
        };
        loop {
            let blank = self.skip_blanks();
            if self.eat("/>") {
                return Ok(element);
            }
            if self.eat(">") {
                break;
            }
            if !blank {
                return Err(self.error("a blank was expected before the attribute"));
            }
            let name = self.name()?;
            self.skip_blanks();
            self.expect("=", "'=' was expected after the attribute name")?;
            self.skip_blanks();
            let value = self.attribute_value()?;
            if element.attribute(&name).is_some() {
                return Err(self.error("duplicate attribute"));
            }
            element.attributes.push((name, value));
        }
        self.content(&mut element)?;
        Ok(element)
    }

    fn attribute_value(&mut self) -> Result<String, Error> {
        let quote = match self.peek() {
            Some(q @ ('"' | '\'')) => q,
            _ => return Err(self.error("the attribute value isn't quoted")),
        };
        self.pos += 1;
        let mut value = String::new();
        loop {
            match self.peek() {
                None => return Err(self.error("unterminated attribute value")),
                Some(c) if c == quote => {
                    self.pos += 1;
                    return Ok(value);
                }
                Some('<') => return Err(self.error("'<' in an attribute value")),
                Some('&') => value.push(self.reference()?),
                // Blanks in an attribute value read as spaces.
                Some(c) if is_blank(c) => {
                    value.push(' ');
                    self.pos += 1;
                }
                Some(c) => {
                    value.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }
    }

    /// An entity or character reference, from its `&` to its `;`.
    fn reference(&mut self) -> Result<char, Error> {
        let start = self.pos;
        self.pos += 1;
        let body = self.until(";", "unterminated reference")?;
        let c = match body {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => {
                let code = if let Some(hex) = body.strip_prefix("#x") {
                    u32::from_str_radix(hex, 16).ok()
                } else if let Some(dec) = body.strip_prefix('#') {
                    dec.parse::<u32>().ok()
                } else {
                    None
                };
                code.filter(|&c| c != 0).and_then(char::from_u32)
            }
        };
        c.ok_or(Error {
            offset: start,
            message: "unknown or invalid reference",
        })
    }

    /// The content of `element` and its end tag.
    fn content(&mut self, element: &mut Element) -> Result<(), Error> {
        let mut text = String::new();
        loop {
            match self.peek() {
                None => return Err(self.error("unclosed element")),
                Some('&') => text.push(self.reference()?),
                Some('<') => {
                    if self.eat("<!--") {
                        self.until("-->", "unterminated comment")?;
                        continue;
                    }
                    if self.eat("<![CDATA[") {
                        text.push_str(self.until("]]>", "unterminated CDATA section")?);
                        continue;
                    }
                    if self.eat("<?") {
                        self.until("?>", "unterminated processing instruction")?;
                        continue;
                    }
                    if !text.is_empty() {
                        element.children.push(Node::Text(std::mem::take(&mut text)));
                    }
                    if self.eat("</") {
                        if self.name()? != element.name {
                            return Err(self.error("the end tag doesn't match the start tag"));
                        }
                        self.skip_blanks();
                        return self.expect(">", "'>' was expected to end the end tag");
                    }
                    let child = self.element()?;
                    element.children.push(Node::Element(child));
                }
                Some(c) => {
                    text.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, Element, Node};

    #[test]
    fn documents_are_read_as_trees() {
        let root = parse(
            "<?xml version='1.0'?>\n<!-- head -->\n<component>\
             <name>a &amp; b</name><engines exec='x &quot;y&quot;'/>\
             <!-- <engines exec=\"never\"/> --><text><![CDATA[<raw>]]>&#x41;&#66;</text>\
             </component>\n<!-- tail -->\n",
        )
        .unwrap();
        assert_eq!(root.name, "component");
        assert_eq!(root.child_text("name").as_deref(), Some("a & b"));
        let engines: Vec<&Element> = root.elements_named("engines").collect();
        assert_eq!(engines.len(), 1);
        assert_eq!(engines[0].attribute("exec"), Some("x \"y\""));
        assert!(engines[0].children.is_empty());
        assert_eq!(root.child_text("text").as_deref(), Some("<raw>AB"));
    }

    #[test]
    fn attribute_blanks_read_as_spaces() {
        let root = parse("<a b=\"1\t2\n3\" c='&#10;'/>").unwrap();
        assert_eq!(root.attribute("b"), Some("1 2 3"));
        assert_eq!(root.attribute("c"), Some("\n"));
        assert_eq!(root.children, Vec::<Node>::new());
    }

    #[test]
    fn documents_that_are_not_well_formed_are_refused() {
        for document in [
            "",
            "   ",
            "text",
            "<a>",
            "<a></b>",
            "<a><b></a></b>",
            "<a/><b/>",
            "<a/>text",
            "<a>&nbsp;</a>",
            "<a>&#0;</a>",
            "<a b=c/>",
            "<a b='1' b='2'/>",
            "<a b='<'/>",
            "<a b='1'c='2'/>",
            "<!-- open",
            "<engines><engine><name>x</name>",
        ] {
            assert!(parse(document).is_err(), "{document:?}");
        }
    }
}
