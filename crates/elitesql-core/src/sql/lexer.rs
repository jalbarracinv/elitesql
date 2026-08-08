use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
    Ident(String),
    Str(String),
    Int(i64),
    Float(f64),
    Blob(Vec<u8>),
    PositionalParam,
    NamedParam(String),
    LParen,
    RParen,
    Comma,
    Dot,
    Star,
    Semi,
    Eq,
    Neq,
    Lt,
    Le,
    Gt,
    Ge,
    // Lexed only to produce clear "not supported" errors in the parser.
    Plus,
    Minus,
    Slash,
    Percent,
}

#[derive(Debug, Clone)]
pub(crate) struct Lexed {
    pub tok: Tok,
    /// Byte offset in the input, for error messages.
    pub pos: usize,
}

fn err(pos: usize, msg: &str) -> Error {
    Error::Sql(format!("{msg} (at byte {pos})"))
}

pub(crate) fn lex(sql: &str) -> Result<Vec<Lexed>> {
    let b = sql.as_bytes();
    let mut i = 0usize;
    let mut out = Vec::new();
    while i < b.len() {
        let start = i;
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => {
                i += 1;
            }
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                if i + 1 >= b.len() {
                    return Err(err(start, "unterminated block comment"));
                }
                i += 2;
            }
            b'(' => {
                out.push(Lexed {
                    tok: Tok::LParen,
                    pos: start,
                });
                i += 1;
            }
            b')' => {
                out.push(Lexed {
                    tok: Tok::RParen,
                    pos: start,
                });
                i += 1;
            }
            b',' => {
                out.push(Lexed {
                    tok: Tok::Comma,
                    pos: start,
                });
                i += 1;
            }
            b'.' => {
                out.push(Lexed {
                    tok: Tok::Dot,
                    pos: start,
                });
                i += 1;
            }
            b'*' => {
                out.push(Lexed {
                    tok: Tok::Star,
                    pos: start,
                });
                i += 1;
            }
            b';' => {
                out.push(Lexed {
                    tok: Tok::Semi,
                    pos: start,
                });
                i += 1;
            }
            b'+' => {
                out.push(Lexed {
                    tok: Tok::Plus,
                    pos: start,
                });
                i += 1;
            }
            b'-' => {
                out.push(Lexed {
                    tok: Tok::Minus,
                    pos: start,
                });
                i += 1;
            }
            b'/' => {
                out.push(Lexed {
                    tok: Tok::Slash,
                    pos: start,
                });
                i += 1;
            }
            b'%' => {
                if i + 1 < b.len() && b[i + 1] == b's' {
                    out.push(Lexed {
                        tok: Tok::PositionalParam,
                        pos: start,
                    });
                    i += 2;
                } else if i + 2 < b.len() && b[i + 1] == b'(' {
                    let mut end = i + 2;
                    if end >= b.len() || !(b[end].is_ascii_alphabetic() || b[end] == b'_') {
                        return Err(err(start, "invalid named parameter"));
                    }
                    end += 1;
                    while end < b.len() && (b[end].is_ascii_alphanumeric() || b[end] == b'_') {
                        end += 1;
                    }
                    if end + 1 >= b.len() || b[end] != b')' || b[end + 1] != b's' {
                        return Err(err(start, "named parameter must use %(name)s syntax"));
                    }
                    let name = std::str::from_utf8(&b[i + 2..end]).expect("ascii parameter");
                    out.push(Lexed {
                        tok: Tok::NamedParam(name.to_owned()),
                        pos: start,
                    });
                    i = end + 2;
                } else {
                    out.push(Lexed {
                        tok: Tok::Percent,
                        pos: start,
                    });
                    i += 1;
                }
            }
            b'?' => {
                out.push(Lexed {
                    tok: Tok::PositionalParam,
                    pos: start,
                });
                i += 1;
            }
            b'=' => {
                out.push(Lexed {
                    tok: Tok::Eq,
                    pos: start,
                });
                i += 1;
            }
            b'!' => {
                if i + 1 < b.len() && b[i + 1] == b'=' {
                    out.push(Lexed {
                        tok: Tok::Neq,
                        pos: start,
                    });
                    i += 2;
                } else {
                    return Err(err(start, "unexpected '!'"));
                }
            }
            b'<' => {
                if i + 1 < b.len() && b[i + 1] == b'=' {
                    out.push(Lexed {
                        tok: Tok::Le,
                        pos: start,
                    });
                    i += 2;
                } else if i + 1 < b.len() && b[i + 1] == b'>' {
                    out.push(Lexed {
                        tok: Tok::Neq,
                        pos: start,
                    });
                    i += 2;
                } else {
                    out.push(Lexed {
                        tok: Tok::Lt,
                        pos: start,
                    });
                    i += 1;
                }
            }
            b'>' => {
                if i + 1 < b.len() && b[i + 1] == b'=' {
                    out.push(Lexed {
                        tok: Tok::Ge,
                        pos: start,
                    });
                    i += 2;
                } else {
                    out.push(Lexed {
                        tok: Tok::Gt,
                        pos: start,
                    });
                    i += 1;
                }
            }
            b'\'' => {
                let (s, next) = lex_string(b, i)?;
                out.push(Lexed {
                    tok: Tok::Str(s),
                    pos: start,
                });
                i = next;
            }
            b'x' | b'X' if i + 1 < b.len() && b[i + 1] == b'\'' => {
                let (bytes, next) = lex_hex_blob(b, i)?;
                out.push(Lexed {
                    tok: Tok::Blob(bytes),
                    pos: start,
                });
                i = next;
            }
            b'0'..=b'9' => {
                let (tok, next) = lex_number(b, i)?;
                out.push(Lexed { tok, pos: start });
                i = next;
            }
            b'A'..=b'Z' | b'a'..=b'z' | b'_' => {
                let mut j = i + 1;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                    j += 1;
                }
                let word = std::str::from_utf8(&b[i..j]).expect("ascii ident");
                out.push(Lexed {
                    tok: Tok::Ident(word.to_owned()),
                    pos: start,
                });
                i = j;
            }
            _ => {
                return Err(err(
                    start,
                    &format!("unexpected character '{}'", (c as char).escape_default()),
                ))
            }
        }
    }
    Ok(out)
}

fn lex_string(b: &[u8], start: usize) -> Result<(String, usize)> {
    let mut i = start + 1;
    let mut bytes = Vec::new();
    loop {
        if i >= b.len() {
            return Err(err(start, "unterminated string literal"));
        }
        if b[i] == b'\'' {
            if i + 1 < b.len() && b[i + 1] == b'\'' {
                bytes.push(b'\'');
                i += 2;
            } else {
                i += 1;
                break;
            }
        } else {
            bytes.push(b[i]);
            i += 1;
        }
    }
    let s = String::from_utf8(bytes).map_err(|_| err(start, "invalid utf8 in string literal"))?;
    Ok((s, i))
}

fn lex_hex_blob(b: &[u8], start: usize) -> Result<(Vec<u8>, usize)> {
    // start is at 'x' / 'X'; start+1 is the quote.
    let mut i = start + 2;
    let hex_start = i;
    while i < b.len() && b[i] != b'\'' {
        i += 1;
    }
    if i >= b.len() {
        return Err(err(start, "unterminated blob literal"));
    }
    let hex = &b[hex_start..i];
    if !hex.len().is_multiple_of(2) {
        return Err(err(
            start,
            "blob literal needs an even number of hex digits",
        ));
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for pair in hex.chunks(2) {
        let hi =
            hex_digit(pair[0]).ok_or_else(|| err(start, "invalid hex digit in blob literal"))?;
        let lo =
            hex_digit(pair[1]).ok_or_else(|| err(start, "invalid hex digit in blob literal"))?;
        bytes.push(hi << 4 | lo);
    }
    Ok((bytes, i + 1))
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn lex_number(b: &[u8], start: usize) -> Result<(Tok, usize)> {
    let mut i = start;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let mut is_float = false;
    if i + 1 < b.len() && b[i] == b'.' && b[i + 1].is_ascii_digit() {
        is_float = true;
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            is_float = true;
            i = j;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        }
    }
    let text = std::str::from_utf8(&b[start..i]).expect("ascii number");
    if is_float {
        let f: f64 = text
            .parse()
            .map_err(|_| err(start, "invalid float literal"))?;
        Ok((Tok::Float(f), i))
    } else {
        let n: i64 = text
            .parse()
            .map_err(|_| err(start, "integer literal out of range"))?;
        Ok((Tok::Int(n), i))
    }
}
