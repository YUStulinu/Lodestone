//! Tokens. Keywords are recognised by the parser from identifiers, so any word that is not used
//! as a keyword in its position can still name a column.

use super::types::{Result, err};

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// An unquoted word, kept as written (compared case-insensitively).
    Word(String),
    /// A "quoted identifier", case kept.
    Quoted(String),
    Str(String),
    Int(String),
    Float(String),
    Param(usize),
    Sym(&'static str),
}

const SYMBOLS: [&str; 22] = [
    "<>", "<=", ">=", "!=", "||", "::", "==", "(", ")", ",", ";", "+", "-", "*", "/", "%", "=", "<", ">", ".", "~", "&",
];

pub fn lex(sql: &str) -> Result<Vec<Token>> {
    Ok(lex_with_pos(sql)?.into_iter().map(|(t, _)| t).collect())
}

/// Tokens with the byte offset where each starts.
pub fn lex_with_pos(sql: &str) -> Result<Vec<(Token, usize)>> {
    let chars: Vec<char> = sql.chars().collect();
    let offsets: Vec<usize> = sql.char_indices().map(|(b, _)| b).chain(std::iter::once(sql.len())).collect();
    let mut toks: Vec<(Token, usize)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let start = offsets[i];
        let mut out: Vec<Token> = Vec::new();
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && chars.get(i + 1) == Some(&'-') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
        } else if c == '\'' {
            let mut s = String::new();
            i += 1;
            loop {
                match chars.get(i) {
                    None => return err!("42601", "unterminated string literal"),
                    Some('\'') if chars.get(i + 1) == Some(&'\'') => {
                        s.push('\'');
                        i += 2;
                    }
                    Some('\'') => {
                        i += 1;
                        break;
                    }
                    Some(&ch) => {
                        s.push(ch);
                        i += 1;
                    }
                }
            }
            out.push(Token::Str(s));
        } else if c == '"' || c == '`' || c == '[' {
            let close = if c == '[' { ']' } else { c };
            let mut s = String::new();
            i += 1;
            loop {
                match chars.get(i) {
                    None => return err!("42601", "unterminated quoted identifier"),
                    Some(&ch) if ch == close && chars.get(i + 1) == Some(&close) && close != ']' => {
                        s.push(close);
                        i += 2;
                    }
                    Some(&ch) if ch == close => {
                        i += 1;
                        break;
                    }
                    Some(&ch) => {
                        s.push(ch);
                        i += 1;
                    }
                }
            }
            out.push(Token::Quoted(s));
        } else if c.is_ascii_digit() || (c == '.' && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let start = i;
            let mut float = false;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            if chars.get(i) == Some(&'.') {
                float = true;
                i += 1;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if matches!(chars.get(i), Some('e' | 'E'))
                && (chars.get(i + 1).is_some_and(|d| d.is_ascii_digit())
                    || (matches!(chars.get(i + 1), Some('+' | '-')) && chars.get(i + 2).is_some_and(|d| d.is_ascii_digit())))
            {
                float = true;
                i += 2;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
            let text: String = chars[start..i].iter().collect();
            out.push(if float { Token::Float(text) } else { Token::Int(text) });
        } else if c == '$' && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit()) {
            i += 1;
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            let n: usize = chars[start..i].iter().collect::<String>().parse().unwrap();
            if n == 0 {
                return err!("42P02", "there is no parameter $0");
            }
            out.push(Token::Param(n));
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$') {
                i += 1;
            }
            out.push(Token::Word(chars[start..i].iter().collect()));
        } else {
            let rest: String = chars[i..chars.len().min(i + 2)].iter().collect();
            match SYMBOLS.iter().find(|s| rest.starts_with(**s)) {
                Some(s) => {
                    out.push(Token::Sym(s));
                    i += s.chars().count();
                }
                None => return err!("42601", "syntax error at or near \"{c}\""),
            }
        }
        toks.extend(out.into_iter().map(|t| (t, start)));
    }
    Ok(toks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens() {
        let t = lex("SELECT a.\"B\", 'it''s', 1.5e3, .5, 42, $2 FROM t -- c\n WHERE x<>y /* z */ || ::").unwrap();
        assert_eq!(
            t,
            vec![
                Token::Word("SELECT".into()),
                Token::Word("a".into()),
                Token::Sym("."),
                Token::Quoted("B".into()),
                Token::Sym(","),
                Token::Str("it's".into()),
                Token::Sym(","),
                Token::Float("1.5e3".into()),
                Token::Sym(","),
                Token::Float(".5".into()),
                Token::Sym(","),
                Token::Int("42".into()),
                Token::Sym(","),
                Token::Param(2),
                Token::Word("FROM".into()),
                Token::Word("t".into()),
                Token::Word("WHERE".into()),
                Token::Word("x".into()),
                Token::Sym("<>"),
                Token::Word("y".into()),
                Token::Sym("||"),
                Token::Sym("::"),
            ]
        );
        assert!(lex("'open").is_err());
        assert!(lex("a ? b").is_err());
    }
}
