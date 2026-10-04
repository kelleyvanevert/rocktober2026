//! Resources: data that's better kept in a file than written as code, referenced
//! by name, like `sample("kick.mp3")` or `envelope("pluck")`. They're stored in
//! the `.rock` file, in a folder per kind (`samples/`, `envelopes/`, ...; see
//! `bundle`).

use std::ops::Range;

use crate::lang::{Token, lex};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResourceKind {
    Sample,
    Envelope,
    Modulation,
    Wavetable,
    Midi,
}

impl ResourceKind {
    pub const ALL: [ResourceKind; 5] = [
        ResourceKind::Sample,
        ResourceKind::Envelope,
        ResourceKind::Modulation,
        ResourceKind::Wavetable,
        ResourceKind::Midi,
    ];

    /// The function that references this kind of resource.
    pub fn function(self) -> &'static str {
        match self {
            ResourceKind::Sample => "sample",
            ResourceKind::Envelope => "envelope",
            ResourceKind::Modulation => "modulation",
            ResourceKind::Wavetable => "wavetable",
            ResourceKind::Midi => "midi",
        }
    }

    /// The folder this kind lives in, in a `.rock` file.
    pub fn dir(self) -> &'static str {
        match self {
            ResourceKind::Sample => "samples",
            ResourceKind::Envelope => "envelopes",
            ResourceKind::Modulation => "modulations",
            ResourceKind::Wavetable => "wavetables",
            ResourceKind::Midi => "midi",
        }
    }

    /// The file a name refers to. Samples come in many formats, so their names
    /// include the extension; the other kinds have one format each.
    pub fn file_name(self, name: &str) -> String {
        match self {
            ResourceKind::Sample => name.to_string(),
            ResourceKind::Envelope | ResourceKind::Modulation => format!("{name}.json"),
            ResourceKind::Wavetable => format!("{name}.wav"),
            ResourceKind::Midi => format!("{name}.mid"),
        }
    }

    /// Where the resource is, or would be, in a `.rock` file.
    pub fn bundle_path(self, name: &str) -> String {
        format!("{}/{}", self.dir(), self.file_name(name))
    }

    fn from_function(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.function() == name)
    }
}

/// A resource referenced in the code.
#[derive(Clone, Debug, PartialEq)]
pub struct ResourceRef {
    pub kind: ResourceKind,
    pub name: String,
    /// Byte range of the whole call, like `sample("kick.mp3", 0:01)`, or up to
    /// the name if the call isn't closed yet.
    pub range: Range<usize>,
    /// Any times among the other arguments, in seconds (a sample's start and end).
    pub times: Vec<f64>,
}

/// Every resource referenced in `src`: calls of a resource function whose first
/// argument is a string. Code that doesn't lex (half typed) has none.
pub fn references(src: &str) -> Vec<ResourceRef> {
    let Ok(tokens) = lex(src) else {
        return Vec::new();
    };
    let mut refs = Vec::new();
    for (i, window) in tokens.windows(3).enumerate() {
        let [
            (Token::Ident(function), start),
            (Token::LParen, _),
            (Token::Str(name), at),
        ] = window
        else {
            continue;
        };
        let Some(kind) = ResourceKind::from_function(function) else {
            continue;
        };
        // The name's closing quote; strings have no escapes.
        let mut end = at + name.len() + 2;
        let mut times = Vec::new();
        let mut depth = 1;
        for (token, pos) in &tokens[i + 3..] {
            match token {
                Token::LParen => depth += 1,
                Token::RParen => depth -= 1,
                Token::Duration(seconds) if depth == 1 => times.push(*seconds),
                _ => {}
            }
            if depth == 0 {
                end = pos + 1;
                break;
            }
        }
        refs.push(ResourceRef {
            kind,
            name: name.clone(),
            range: *start..end,
            times,
        });
    }
    refs
}

/// The resource reference that `offset` is in (or right after), if any.
pub fn reference_at(src: &str, offset: usize) -> Option<ResourceRef> {
    // Innermost first: a reference nested in another one's arguments wins.
    references(src)
        .into_iter()
        .filter(|r| r.range.start <= offset && offset <= r.range.end)
        .min_by_key(|r| r.range.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_resource_calls() {
        let src = r#"sample("kick.mp3", 0:01, 0:02:500).gain(-6db) -- envelope("no")
add(envelope("pluck"), wavetable("basic", 0.2))"#;
        let refs = references(src);
        let found: Vec<(&str, &str)> = refs
            .iter()
            .map(|r| (&src[r.range.clone()], r.name.as_str()))
            .collect();
        assert_eq!(
            found,
            [
                (r#"sample("kick.mp3", 0:01, 0:02:500)"#, "kick.mp3"),
                (r#"envelope("pluck")"#, "pluck"),
                (r#"wavetable("basic", 0.2)"#, "basic"),
            ]
        );
        assert_eq!(refs[0].times, [1.0, 2.5]);
        assert_eq!(refs[1].kind, ResourceKind::Envelope);
    }

    #[test]
    fn unclosed_call_ends_at_the_name() {
        let src = r#"sample("kick.mp3", 0:0"#;
        let r = &references(src)[0];
        assert_eq!(&src[r.range.clone()], r#"sample("kick.mp3""#);
    }

    #[test]
    fn innermost_reference_at_offset() {
        let src = r#"sample("a.wav").reverb(sample("b.wav"))"#;
        assert_eq!(reference_at(src, 3).unwrap().name, "a.wav");
        assert_eq!(reference_at(src, 32).unwrap().name, "b.wav");
        assert_eq!(reference_at(src, 16), None);
        assert_eq!(reference_at(src, src.len() - 1).unwrap().name, "b.wav");
        assert_eq!(reference_at("sample(", 3), None);
    }

    #[test]
    fn file_names() {
        assert_eq!(
            ResourceKind::Envelope.bundle_path("pluck"),
            "envelopes/pluck.json"
        );
        assert_eq!(
            ResourceKind::Sample.bundle_path("drums/kick.mp3"),
            "samples/drums/kick.mp3"
        );
    }
}
