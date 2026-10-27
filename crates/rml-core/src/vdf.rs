//! Minimal reader for Valve's text KeyValues format (`.vdf`, `.acf`).
//!
//! Only what Steam writes into `libraryfolders.vdf` and `appmanifest_*.acf`
//! is needed: quoted or bare keys, string values, nested blocks, `//`
//! comments and the usual backslash escapes. Platform conditionals such as
//! `[$WIN32]` are skipped.

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Obj(Vec<(String, Value)>),
}

impl Value {
    /// First child with the given key (case-insensitive, as in Steam).
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(items) => items
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v),
            Value::Str(_) => None,
        }
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Value::Str(s) => Some(s),
            Value::Obj(_) => None,
        }
    }

    pub fn u64(&self, key: &str) -> Option<u64> {
        self.str(key)?.trim().parse().ok()
    }

    pub fn entries(&self) -> &[(String, Value)] {
        match self {
            Value::Obj(items) => items,
            Value::Str(_) => &[],
        }
    }
}

/// Parses a whole document. The result is an object holding the top-level
/// key(s), e.g. `{"AppState": {...}}`.
pub fn parse(text: &str) -> Result<Value> {
    let mut lexer = Lexer {
        chars: text.char_indices().peekable(),
        src: text,
    };
    let items = parse_items(&mut lexer, false)?;
    Ok(Value::Obj(items))
}

#[derive(Debug, PartialEq)]
enum Token {
    Str(String),
    Open,
    Close,
}

struct Lexer<'a> {
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
    src: &'a str,
}

impl Lexer<'_> {
    fn err(&mut self, detail: &str) -> Error {
        let pos = self.chars.peek().map(|(i, _)| *i).unwrap_or(self.src.len());
        let line = self.src[..pos].matches('\n').count() + 1;
        Error::Parse {
            what: "VDF".into(),
            detail: format!("{detail} (line {line})"),
        }
    }

    fn next(&mut self) -> Result<Option<Token>> {
        loop {
            let Some(&(_, c)) = self.chars.peek() else {
                return Ok(None);
            };
            match c {
                c if c.is_whitespace() => {
                    self.chars.next();
                }
                '/' => {
                    self.chars.next();
                    if matches!(self.chars.peek(), Some((_, '/'))) {
                        for (_, c) in self.chars.by_ref() {
                            if c == '\n' {
                                break;
                            }
                        }
                    } else {
                        return Ok(Some(Token::Str(self.bare("/".into()))));
                    }
                }
                '[' => {
                    // Conditional like [$WIN32]; ignore it entirely.
                    for (_, c) in self.chars.by_ref() {
                        if c == ']' {
                            break;
                        }
                    }
                }
                '{' => {
                    self.chars.next();
                    return Ok(Some(Token::Open));
                }
                '}' => {
                    self.chars.next();
                    return Ok(Some(Token::Close));
                }
                '"' => {
                    self.chars.next();
                    let mut s = String::new();
                    loop {
                        match self.chars.next() {
                            None => return Err(self.err("unterminated string")),
                            Some((_, '"')) => break,
                            Some((_, '\\')) => match self.chars.next() {
                                Some((_, 'n')) => s.push('\n'),
                                Some((_, 't')) => s.push('\t'),
                                Some((_, other)) => s.push(other),
                                None => return Err(self.err("unterminated escape")),
                            },
                            Some((_, ch)) => s.push(ch),
                        }
                    }
                    return Ok(Some(Token::Str(s)));
                }
                _ => return Ok(Some(Token::Str(self.bare(String::new())))),
            }
        }
    }

    fn bare(&mut self, mut s: String) -> String {
        while let Some(&(_, c)) = self.chars.peek() {
            if c.is_whitespace() || c == '{' || c == '}' || c == '"' {
                break;
            }
            s.push(c);
            self.chars.next();
        }
        s
    }
}

fn parse_items(lexer: &mut Lexer, nested: bool) -> Result<Vec<(String, Value)>> {
    let mut items = Vec::new();
    loop {
        let key = match lexer.next()? {
            None if nested => return Err(lexer.err("missing closing brace")),
            None => return Ok(items),
            Some(Token::Close) if nested => return Ok(items),
            Some(Token::Close) => return Err(lexer.err("unexpected closing brace")),
            Some(Token::Open) => return Err(lexer.err("block without key")),
            Some(Token::Str(k)) => k,
        };
        let value = match lexer.next()? {
            Some(Token::Str(v)) => Value::Str(v),
            Some(Token::Open) => Value::Obj(parse_items(lexer, true)?),
            Some(Token::Close) | None => {
                return Err(lexer.err(&format!("key `{key}` has no value")))
            }
        };
        items.push((key, value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#""AppState"
{
	"appid"		"2676030"
	"name"		"Runehaven"
	"StateFlags"		"4"
	"installdir"		"Runehaven"
	"buildid"		"23702508"
	"InstalledDepots"
	{
		"2676031"
		{
			"manifest"		"7388440364521527929"
			"size"		"345226541"
		}
	}
	"UserConfig"
	{
	}
}
"#;

    #[test]
    fn parses_app_manifest() {
        let doc = parse(MANIFEST).unwrap();
        let app = doc.get("AppState").unwrap();
        assert_eq!(app.str("appid"), Some("2676030"));
        assert_eq!(app.u64("stateflags"), Some(4));
        let depot = app.get("InstalledDepots").unwrap().get("2676031").unwrap();
        assert_eq!(depot.str("manifest"), Some("7388440364521527929"));
        assert_eq!(app.get("UserConfig").unwrap().entries().len(), 0);
    }

    #[test]
    fn handles_escapes_comments_and_conditionals() {
        let doc = parse(
            "// comment\n\"a\" { \"path\" \"C:\\\\Games\\\" x\" \"b\" \"1\" [$WIN32] bare value }",
        )
        .unwrap();
        let a = doc.get("a").unwrap();
        assert_eq!(a.str("path"), Some("C:\\Games\" x"));
        assert_eq!(a.str("bare"), Some("value"));
    }

    #[test]
    fn reports_broken_input() {
        assert!(parse("\"a\" { \"b\" \"c\"").is_err());
        assert!(parse("\"a\"").is_err());
        assert!(parse("\"a\" \"unterminated").is_err());
    }
}
