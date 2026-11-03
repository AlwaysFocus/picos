//! A bounded view of the bytes currently in guest RAM.
//!
//! This decodes instruction-shaped bytes, not original source. Unified RAM can
//! contain code or data at any address, and valid instruction encodings can also
//! occur inside data. Raw bytes remain alongside every decoded row.
use crate::vm::{Machine, NAMES, is_unary, operand_count};
use serde_json::{Value, json};

fn decode(bytes: &[u8]) -> Option<String> {
    if bytes.len() != 8 {
        return None;
    }
    let opcode = bytes[0];
    let register = bytes[1];
    let mode = bytes[2];
    let raw = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    // Match the CPU's actual validation, including its validation of fields
    // that a particular opcode subsequently ignores.
    if opcode as usize >= NAMES.len()
        || register > 15
        || mode > 1
        || bytes[3] != 0
        || (mode == 1 && raw > 15)
    {
        return None;
    }
    let name = NAMES[opcode as usize];
    if operand_count(opcode) == 0 {
        return Some(name.into());
    }
    if opcode == 29 {
        return Some(format!("{name} r{register}"));
    }
    let operand = if mode == 1 {
        format!("r{raw}")
    } else if matches!(opcode, 14..=17 | 19..=26) {
        format!("0x{raw:08x}")
    } else {
        raw.to_string()
    };
    Some(if is_unary(opcode) {
        format!("{name} {operand}")
    } else {
        format!("{name} r{register}, {operand}")
    })
}

/// Decode a RAM window beginning at exactly `address`, with no alignment guess.
/// Whole rows contain eight bytes; a final partial row is rendered as data.
pub fn disassemble(machine: &Machine, address: usize, length: usize) -> Result<Value, String> {
    if !(8..=4096).contains(&length) {
        return Err("Disassembly length must be 8..4096 bytes".into());
    }
    let memory = machine.read_memory(address, length)?;
    let rows: Vec<Value> = memory
        .chunks(8)
        .enumerate()
        .map(|(index, bytes)| {
            let row_address = address + index * 8;
            let decoded = decode(bytes);
            let kind = if decoded.is_some() {
                "instruction"
            } else {
                "data"
            };
            let assembly = decoded.unwrap_or_else(|| {
                format!(
                    ".byte {}",
                    bytes
                        .iter()
                        .map(|byte| format!("0x{byte:02x}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            });
            json!({
                "address":row_address,
                "bytes":bytes,
                "assembly":assembly,
                "kind":kind,
                "isPc":row_address == machine.pc as usize,
            })
        })
        .collect();
    Ok(json!({
        "address":address,
        "length":length,
        "pc":machine.pc,
        "ramBytes":machine.ram.len(),
        "videoBase":machine.video_base,
        "registers":machine.registers,
        "rows":rows,
    }))
}
