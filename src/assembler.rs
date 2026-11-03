use crate::vm::{Config, NAMES, VIDEO_BYTES, VIDEO_HEIGHT, VIDEO_WIDTH, is_unary, operand_count};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug)]
pub struct Assembly {
    pub bytes: Vec<u8>,
    pub symbols: BTreeMap<String, u32>,
    pub listing: Vec<Value>,
}

fn identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}
fn uncomment(line: &str) -> &str {
    let mut quote = None;
    let mut escaped = false;
    let mut chars = line.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        if quote.is_some() && ch == '\\' {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
        } else if ch == ';' || (ch == '/' && chars.peek().is_some_and(|(_, c)| *c == '/')) {
            return line[..i].trim();
        }
    }
    line.trim()
}
fn arguments(text: &str) -> Result<Vec<String>, String> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    for (i, ch) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quote.is_some() && ch == '\\' {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
        } else if ch == ',' {
            result.push(text[start..i].trim().to_owned());
            start = i + 1;
        }
    }
    if quote.is_some() {
        return Err("Unterminated string literal".into());
    }
    result.push(text[start..].trim().to_owned());
    if result.iter().any(|s| s.is_empty()) {
        return Err("Empty operand".into());
    }
    Ok(result)
}
fn character(text: &str) -> Option<i64> {
    let inner = text.strip_prefix('\'')?.strip_suffix('\'')?;
    if inner.starts_with('\\') {
        return match inner {
            "\\n" => Some(10),
            "\\r" => Some(13),
            "\\t" => Some(9),
            "\\0" => Some(0),
            "\\'" => Some(39),
            "\\\\" => Some(92),
            _ => None,
        };
    }
    let mut chars = inner.chars();
    let ch = chars.next()?;
    if chars.next().is_none() && ch != '\'' {
        Some(ch as i64)
    } else {
        None
    }
}

#[derive(Debug, Clone)]
enum Token {
    Number(i64),
    Symbol(String),
    Plus,
    Minus,
    Open,
    Close,
}
fn tokens(text: &str) -> Result<Vec<Token>, String> {
    let bytes = text.as_bytes();
    let mut output = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        match c {
            b'+' => {
                output.push(Token::Plus);
                i += 1;
            }
            b'-' => {
                output.push(Token::Minus);
                i += 1;
            }
            b'(' => {
                output.push(Token::Open);
                i += 1;
            }
            b')' => {
                output.push(Token::Close);
                i += 1;
            }
            b'0'..=b'9' => {
                let start = i;
                let radix = if c == b'0' && i + 1 < bytes.len() {
                    match bytes[i + 1] {
                        b'x' | b'X' => 16,
                        b'b' | b'B' => 2,
                        _ => 10,
                    }
                } else {
                    10
                };
                if radix != 10 {
                    i += 2;
                }
                let digits = i;
                while i < bytes.len()
                    && match radix {
                        16 => bytes[i].is_ascii_hexdigit(),
                        2 => matches!(bytes[i], b'0' | b'1'),
                        _ => bytes[i].is_ascii_digit(),
                    }
                {
                    i += 1;
                }
                if i == digits {
                    return Err(format!("Invalid number near {}", &text[start..]));
                }
                let value = i64::from_str_radix(&text[digits..i], radix)
                    .map_err(|_| "Integer expression overflow")?;
                output.push(Token::Number(value));
            }
            _ if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                i += 1;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                output.push(Token::Symbol(text[start..i].into()));
            }
            _ => return Err(format!("Invalid expression: {text}")),
        }
    }
    Ok(output)
}
struct Symbols {
    known: BTreeMap<String, i64>,
    constants: BTreeMap<String, String>,
}
impl Symbols {
    fn define(&mut self, name: &str, value: i64) -> Result<(), String> {
        if self.known.contains_key(name) || self.constants.contains_key(name) {
            return Err(format!("Duplicate symbol: {name}"));
        }
        self.known.insert(name.into(), value);
        Ok(())
    }
    fn constant(&mut self, name: &str, expression: &str) -> Result<(), String> {
        if !identifier(name) {
            return Err(format!("Invalid symbol: {name}"));
        }
        if self.known.contains_key(name) || self.constants.contains_key(name) {
            return Err(format!("Duplicate symbol: {name}"));
        }
        self.constants.insert(name.into(), expression.into());
        Ok(())
    }
    fn lookup(
        &self,
        name: &str,
        active: &mut BTreeSet<String>,
        depth: usize,
    ) -> Result<i64, String> {
        if let Some(value) = self.known.get(name) {
            return Ok(*value);
        }
        let expression = self
            .constants
            .get(name)
            .ok_or_else(|| format!("Unknown symbol: {name}"))?;
        if active.contains(name) {
            return Err(format!("Circular constant: {name}"));
        }
        active.insert(name.into());
        let result = self.evaluate_inner(expression, active, depth + 1);
        active.remove(name);
        result
    }
    fn evaluate(&self, expression: &str) -> Result<i64, String> {
        self.evaluate_inner(expression, &mut BTreeSet::new(), 0)
    }
    fn evaluate_inner(
        &self,
        expression: &str,
        active: &mut BTreeSet<String>,
        depth: usize,
    ) -> Result<i64, String> {
        if depth > 128 {
            return Err("Expression nesting exceeds 128 levels".into());
        }
        let text = expression.trim();
        if let Some(ch) = character(text) {
            return Ok(ch);
        }
        let tokens = tokens(text)?;
        let mut parser = Parser {
            tokens: &tokens,
            position: 0,
            symbols: self,
            active,
        };
        let result = parser.sum(depth)?;
        if parser.position != tokens.len() {
            return Err(format!("Invalid integer expression: {text}"));
        }
        Ok(result)
    }
}
struct Parser<'a> {
    tokens: &'a [Token],
    position: usize,
    symbols: &'a Symbols,
    active: &'a mut BTreeSet<String>,
}
impl Parser<'_> {
    fn atom(&mut self, depth: usize) -> Result<i64, String> {
        if depth > 128 {
            return Err("Expression nesting exceeds 128 levels".into());
        }
        let token = self
            .tokens
            .get(self.position)
            .cloned()
            .ok_or("Incomplete expression")?;
        self.position += 1;
        match token {
            Token::Number(n) => Ok(n),
            Token::Symbol(name) => self.symbols.lookup(&name, self.active, depth + 1),
            Token::Plus => self.atom(depth + 1),
            Token::Minus => self
                .atom(depth + 1)?
                .checked_neg()
                .ok_or("Integer expression overflow".into()),
            Token::Open => {
                let value = self.sum(depth + 1)?;
                if !matches!(self.tokens.get(self.position), Some(Token::Close)) {
                    return Err("Missing closing parenthesis".into());
                }
                self.position += 1;
                Ok(value)
            }
            Token::Close => Err("Unexpected closing parenthesis".into()),
        }
    }
    fn sum(&mut self, depth: usize) -> Result<i64, String> {
        let mut value = self.atom(depth)?;
        loop {
            let plus = match self.tokens.get(self.position) {
                Some(Token::Plus) => true,
                Some(Token::Minus) => false,
                _ => break,
            };
            self.position += 1;
            let rhs = self.atom(depth)?;
            value = if plus {
                value.checked_add(rhs)
            } else {
                value.checked_sub(rhs)
            }
            .ok_or("Integer expression overflow")?;
        }
        Ok(value)
    }
}
fn word(value: i64) -> Result<u32, String> {
    if !(-2_147_483_648..=4_294_967_295).contains(&value) {
        Err(format!("Value does not fit 32 bits: {value}"))
    } else {
        Ok(value as u32)
    }
}
fn register(text: &str) -> Result<u8, String> {
    let valid = text.len() >= 2
        && matches!(text.as_bytes()[0], b'r' | b'R')
        && text.as_bytes()[1..].iter().all(u8::is_ascii_digit);
    let parsed = if valid {
        text[1..].parse::<u8>().ok()
    } else {
        None
    };
    if let Some(value) = parsed.filter(|n| *n < 16 && text.len() == 1 + n.to_string().len()) {
        Ok(value)
    } else {
        Err(format!("Expected register r0..r15, received {text}"))
    }
}
fn is_register_shape(text: &str) -> bool {
    text.len() >= 2
        && matches!(text.as_bytes()[0], b'r' | b'R')
        && text.as_bytes()[1..].iter().all(u8::is_ascii_digit)
}
struct Statement {
    name: String,
    args: Vec<String>,
    address: usize,
    data: Vec<u8>,
    line: usize,
}

pub fn assemble(source: &str, config: &Config) -> Result<Assembly, String> {
    config.validate()?;
    if source.len() > 1_048_576 {
        return Err("Assembly source exceeds 1 MiB".into());
    }
    let mut symbols = Symbols {
        known: BTreeMap::from([
            ("RAM_BYTES".into(), config.ram_bytes as i64),
            ("DISK_BYTES".into(), config.disk_bytes as i64),
            ("VIDEO_BASE".into(), (config.ram_bytes - VIDEO_BYTES) as i64),
            ("VIDEO_WIDTH".into(), VIDEO_WIDTH as i64),
            ("VIDEO_HEIGHT".into(), VIDEO_HEIGHT as i64),
        ]),
        constants: BTreeMap::new(),
    };
    let mut statements = Vec::new();
    let mut listing = Vec::new();
    let mut address = 0usize;
    let capacity = (config.ram_bytes - VIDEO_BYTES).min(config.disk_bytes);
    for (index, original) in source.lines().enumerate() {
        let result = (|| -> Result<(), String> {
            let mut line = uncomment(original);
            if line.is_empty() {
                return Ok(());
            }
            loop {
                let Some(colon) = line.find(':') else {
                    break;
                };
                if !identifier(&line[..colon]) {
                    break;
                }
                symbols.define(&line[..colon], address as i64)?;
                line = line[colon + 1..].trim();
            }
            if line.is_empty() {
                return Ok(());
            }
            let split = line.find(char::is_whitespace).unwrap_or(line.len());
            let name = line[..split].to_ascii_lowercase();
            let tail = line[split..].trim();
            if name == ".equ" {
                let end = tail
                    .find(|c: char| c.is_whitespace() || c == ',')
                    .ok_or("Use .equ NAME, expression")?;
                let ident = &tail[..end];
                let expression = tail[end..]
                    .trim()
                    .strip_prefix(',')
                    .unwrap_or(tail[end..].trim())
                    .trim();
                if expression.is_empty() {
                    return Err("Use .equ NAME, expression".into());
                }
                symbols.constant(ident, expression)?;
                return Ok(());
            }
            let args = arguments(tail)?;
            let mut data = Vec::new();
            let size = match name.as_str() {
                ".ascii" | ".asciz" => {
                    if args.len() != 1 {
                        return Err(format!("{name} expects one string"));
                    }
                    data = serde_json::from_str::<String>(&args[0])
                        .map_err(|_| "Invalid string escape or literal")?
                        .into_bytes();
                    data.len() + usize::from(name == ".asciz")
                }
                ".byte" | ".word" => {
                    if args.is_empty() {
                        return Err(format!("{name} expects values"));
                    }
                    args.len() * if name == ".byte" { 1 } else { 4 }
                }
                ".space" => {
                    if args.len() != 1 {
                        return Err(".space expects a byte count".into());
                    }
                    usize::try_from(symbols.evaluate(&args[0])?)
                        .map_err(|_| ".space size must be a nonnegative integer")?
                }
                _ => {
                    let opcode = NAMES
                        .iter()
                        .position(|n| *n == name)
                        .ok_or_else(|| format!("Unknown instruction/directive: {name}"))?
                        as u8;
                    let count = operand_count(opcode);
                    if args.len() != count {
                        return Err(format!(
                            "{name} expects {count} operand(s), received {}",
                            args.len()
                        ));
                    }
                    8
                }
            };
            if address.checked_add(size).is_none_or(|n| n > capacity) {
                return Err(format!(
                    "Image exceeds available boot capacity ({capacity} bytes)"
                ));
            }
            listing.push(
                json!({"line": index + 1, "address": address, "size": size, "source": original}),
            );
            statements.push(Statement {
                name,
                args,
                address,
                data,
                line: index + 1,
            });
            address += size;
            Ok(())
        })();
        result.map_err(|e| format!("Line {}: {e}", index + 1))?;
    }
    if address == 0 {
        return Err("Assembly source produces an empty image".into());
    }
    let mut output = vec![0; address];
    for stmt in statements {
        let result = (|| -> Result<(), String> {
            let at = stmt.address;
            match stmt.name.as_str() {
                ".space" => (),
                ".ascii" | ".asciz" => output[at..at + stmt.data.len()].copy_from_slice(&stmt.data),
                ".byte" | ".word" => {
                    for (i, arg) in stmt.args.iter().enumerate() {
                        let value = symbols.evaluate(arg)?;
                        if stmt.name == ".byte" {
                            output[at + i] = u8::try_from(value)
                                .map_err(|_| format!("Value does not fit a byte: {value}"))?;
                        } else {
                            output[at + i * 4..at + i * 4 + 4]
                                .copy_from_slice(&word(value)?.to_le_bytes());
                        }
                    }
                }
                _ => {
                    let opcode = NAMES.iter().position(|n| *n == stmt.name).unwrap() as u8;
                    output[at] = opcode;
                    let operand = if is_unary(opcode) {
                        Some(&stmt.args[0])
                    } else if operand_count(opcode) > 0 {
                        output[at + 1] = register(&stmt.args[0])?;
                        if operand_count(opcode) == 2 {
                            Some(&stmt.args[1])
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    if let Some(operand) = operand {
                        let raw = if is_register_shape(operand) {
                            output[at + 2] = 1;
                            register(operand)? as u32
                        } else {
                            word(symbols.evaluate(operand)?)?
                        };
                        output[at + 4..at + 8].copy_from_slice(&raw.to_le_bytes());
                    }
                }
            }
            Ok(())
        })();
        result.map_err(|e| format!("Line {}: {e}", stmt.line))?;
    }
    let mut resolved = BTreeMap::new();
    for name in symbols.known.keys().chain(symbols.constants.keys()) {
        let value = symbols
            .lookup(name, &mut BTreeSet::new(), 0)
            .map_err(|e| format!("Constant {name}: {e}"))?;
        resolved.insert(name.clone(), word(value)?);
    }
    Ok(Assembly {
        bytes: output,
        symbols: resolved,
        listing,
    })
}
