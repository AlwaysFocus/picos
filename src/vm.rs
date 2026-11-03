use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::VecDeque;

pub const VIDEO_WIDTH: usize = 256;
pub const VIDEO_HEIGHT: usize = 192;
pub const VIDEO_BYTES: usize = VIDEO_WIDTH * VIDEO_HEIGHT;
pub const MAX_RAM_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_DISK_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
pub const NAMES: [&str; 32] = [
    "nop", "halt", "yield", "mov", "add", "sub", "mul", "div", "mod", "and", "or", "xor", "shl",
    "shr", "load8", "load32", "store8", "store32", "cmp", "jmp", "je", "jne", "jlt", "jle", "jgt",
    "jge", "call", "ret", "push", "pop", "in", "out",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub ram_bytes: usize,
    pub disk_bytes: usize,
    pub clock_hz: u64,
    pub max_cycles: u64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            ram_bytes: 8 * 1024 * 1024,
            disk_bytes: 32 * 1024 * 1024,
            clock_hz: 300_000_000,
            max_cycles: 300_000_000,
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<(), String> {
        bounded(
            self.ram_bytes as u64,
            65_536,
            MAX_RAM_BYTES as u64,
            "ramBytes",
        )?;
        bounded(
            self.disk_bytes as u64,
            8,
            MAX_DISK_BYTES as u64,
            "diskBytes",
        )?;
        bounded(self.clock_hz, 1, 10_000_000_000, "clockHz")?;
        bounded(self.max_cycles, 1, MAX_SAFE_INTEGER, "maxCycles")?;
        Ok(())
    }
}
pub fn bounded(value: u64, min: u64, max: u64, label: &str) -> Result<(), String> {
    if value < min || value > max {
        Err(format!("{label} must be an integer from {min} to {max}"))
    } else {
        Ok(())
    }
}
fn range(address: usize, length: usize, capacity: usize, label: &str) -> Result<(), String> {
    if address > capacity
        || length > capacity
        || address.checked_add(length).is_none_or(|end| end > capacity)
    {
        Err(format!(
            "{label} access outside {capacity} bytes: {address} + {length}"
        ))
    } else {
        Ok(())
    }
}
/// Classify only capacity failures emitted by this VM and its cross-assembler.
/// Syntax errors, host request-size limits, and ordinary guest traps are not
/// resource exhaustion. The boot image must fit both code RAM and raw disk.
pub fn resource_limit_error(error: &str) -> Option<&'static str> {
    fn numeric(value: &str) -> bool {
        !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
    }
    for (label, resource) in [
        ("RAM", "ram"),
        ("Boot RAM", "ram"),
        ("Instruction", "ram"),
        ("Call", "ram"),
        ("Disk", "disk"),
        ("Boot disk", "disk"),
        ("Disk cursor", "disk"),
    ] {
        let Some(tail) = error.strip_prefix(&format!("{label} access outside ")) else {
            continue;
        };
        let Some((capacity, access)) = tail.split_once(" bytes: ") else {
            continue;
        };
        let Some((address, length)) = access.split_once(" + ") else {
            continue;
        };
        if numeric(capacity) && numeric(address) && numeric(length) {
            return Some(resource);
        }
    }
    if error == "Stack underflow outside RAM" {
        return Some("ram");
    }
    let image_error = if let Some(line) = error.strip_prefix("Line ") {
        let (number, message) = line.split_once(": ")?;
        if !numeric(number) {
            return None;
        }
        message
    } else {
        error
    };
    if image_error
        .strip_prefix("Image exceeds available boot capacity (")
        .and_then(|value| value.strip_suffix(" bytes)"))
        .is_some_and(numeric)
    {
        return Some("boot");
    }
    None
}

pub fn instruction_cost(opcode: u8) -> u64 {
    match opcode {
        14..=17 | 28..=29 => 2,
        26..=27 => 3,
        6 => 4,
        7..=8 | 30..=31 => 16,
        _ => 1,
    }
}
pub fn operand_count(opcode: u8) -> usize {
    match opcode {
        0..=2 | 27 => 0,
        19..=26 | 28..=29 => 1,
        _ => 2,
    }
}
pub fn is_unary(opcode: u8) -> bool {
    matches!(opcode, 19..=26 | 28)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Boot {
    address: usize,
    disk_offset: usize,
    length: usize,
}

const MAX_MEMORY_CHANGE_RANGES: usize = 64;
const MEMORY_CHANGE_PREVIEW_BYTES: usize = 32;

/// Observer evidence, separate from guest RAM and checkpointed machine state.
/// Counts byte transitions, so changing a byte and restoring it counts twice.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryChanges {
    pub changed_bytes: u64,
    pub ranges: Vec<MemoryChange>,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryChange {
    pub address: usize,
    pub length: usize,
    pub changed_bytes: u64,
    /// The first at most 32 bytes of this changed range, before and after.
    pub before: Vec<u8>,
    pub after: Vec<u8>,
}

impl MemoryChanges {
    fn record_range(
        &mut self,
        address: usize,
        length: usize,
        changed_bytes: u64,
        before: &[u8],
        after: &[u8],
    ) {
        self.changed_bytes = self.changed_bytes.saturating_add(changed_bytes);
        if self.truncated {
            return;
        }
        // Consecutive stores to adjacent bytes (for example painting pixels)
        // remain one range, without retaining an instruction-by-instruction log.
        if let Some(last) = self
            .ranges
            .last_mut()
            .filter(|last| last.address + last.length == address)
        {
            let preview = MEMORY_CHANGE_PREVIEW_BYTES.saturating_sub(last.before.len());
            last.before.extend(before.iter().take(preview));
            last.after.extend(after.iter().take(preview));
            last.length += length;
            last.changed_bytes += changed_bytes;
            return;
        }
        if self.ranges.len() == MAX_MEMORY_CHANGE_RANGES {
            self.truncated = true;
            return;
        }
        self.ranges.push(MemoryChange {
            address,
            length,
            changed_bytes,
            before: before
                .iter()
                .take(MEMORY_CHANGE_PREVIEW_BYTES)
                .copied()
                .collect(),
            after: after
                .iter()
                .take(MEMORY_CHANGE_PREVIEW_BYTES)
                .copied()
                .collect(),
        });
    }

    fn record(&mut self, address: usize, before: &[u8], after: &[u8]) {
        self.record_with(address, before, |offset| after[offset]);
    }

    // Close differences share one range, preserving unchanged bytes in previews.
    // This keeps zero padding in encoded instructions from fragmenting a program
    // into hundreds of one-byte ranges. Counts still include only changed bytes.
    fn record_with(&mut self, address: usize, before: &[u8], after: impl Fn(usize) -> u8) {
        let mut offset = 0;
        while offset < before.len() {
            if before[offset] == after(offset) {
                offset += 1;
                continue;
            }
            let start = offset;
            let mut last_changed = offset;
            let mut changed_bytes = 0;
            while offset < before.len() {
                if before[offset] != after(offset) {
                    last_changed = offset;
                    changed_bytes += 1;
                } else if offset - last_changed > 8 {
                    break;
                }
                offset += 1;
            }
            let length = last_changed + 1 - start;
            let preview_length = length.min(MEMORY_CHANGE_PREVIEW_BYTES);
            let mut after_preview = [0; MEMORY_CHANGE_PREVIEW_BYTES];
            for (index, byte) in after_preview.iter_mut().take(preview_length).enumerate() {
                *byte = after(start + index);
            }
            self.record_range(
                address + start,
                length,
                changed_bytes,
                &before[start..start + preview_length],
                &after_preview[..preview_length],
            );
        }
    }
}

pub struct Machine {
    pub config: Config,
    pub ram: Vec<u8>,
    pub disk: Vec<u8>,
    pub registers: [u32; 16],
    pub pc: u32,
    pub cycles: u64,
    pub instructions: u64,
    pub status: String,
    pub fault: Option<String>,
    pub serial: String,
    pub sealed: bool,
    pub video_base: usize,
    pub ram_touched_bytes: usize,
    pub disk_touched_bytes: usize,
    pub disk_cursor: usize,
    ram_touched: Vec<u8>,
    disk_touched: Vec<u8>,
    comparison: i8,
    keys: VecDeque<u8>,
    boot: Boot,
    serial_count: usize,
    memory_changes: MemoryChanges,
}

impl Machine {
    pub fn new(config: Config) -> Result<Self, String> {
        config.validate()?;
        let mut machine = Self {
            ram: vec![0; config.ram_bytes],
            disk: vec![0; config.disk_bytes],
            ram_touched: vec![0; config.ram_bytes.div_ceil(8)],
            disk_touched: vec![0; config.disk_bytes.div_ceil(8)],
            video_base: config.ram_bytes - VIDEO_BYTES,
            config,
            registers: [0; 16],
            pc: 0,
            cycles: 0,
            instructions: 0,
            status: "ready".into(),
            fault: None,
            serial: String::new(),
            sealed: false,
            ram_touched_bytes: 0,
            disk_touched_bytes: 0,
            disk_cursor: 0,
            comparison: 0,
            keys: VecDeque::new(),
            boot: Boot {
                address: 0,
                disk_offset: 0,
                length: 0,
            },
            serial_count: 0,
            memory_changes: MemoryChanges::default(),
        };
        machine.reset()?;
        Ok(machine)
    }
    fn mark(map: &mut [u8], count: &mut usize, address: usize, length: usize) {
        for i in address..address + length {
            let mask = 1 << (i & 7);
            if map[i >> 3] & mask == 0 {
                map[i >> 3] |= mask;
                *count += 1;
            }
        }
    }
    fn writable(&self) -> Result<(), String> {
        if self.sealed {
            Err("Machine is sealed: external code and byte injection are disabled".into())
        } else {
            Ok(())
        }
    }
    pub fn load(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.load_at(bytes, 0, 0)
    }
    pub fn load_at(
        &mut self,
        bytes: &[u8],
        address: usize,
        disk_offset: usize,
    ) -> Result<(), String> {
        self.writable()?;
        if bytes.is_empty() {
            return Err("Boot image is empty".into());
        }
        range(address, bytes.len(), self.video_base, "Boot RAM")?;
        range(disk_offset, bytes.len(), self.disk.len(), "Boot disk")?;
        self.disk[disk_offset..disk_offset + bytes.len()].copy_from_slice(bytes);
        Self::mark(
            &mut self.disk_touched,
            &mut self.disk_touched_bytes,
            disk_offset,
            bytes.len(),
        );
        self.boot = Boot {
            address,
            disk_offset,
            length: bytes.len(),
        };
        self.reset()
    }
    pub fn reset(&mut self) -> Result<(), String> {
        // Reset/load are atomic host operations: show old RAM -> final boot RAM,
        // never a misleading intermediate zero image. No second RAM allocation.
        let Boot {
            address,
            disk_offset,
            length,
        } = self.boot;
        self.memory_changes.record_with(0, &self.ram, |offset| {
            if (address..address + length).contains(&offset) {
                self.disk[disk_offset + offset - address]
            } else {
                0
            }
        });
        self.ram.fill(0);
        self.ram_touched.fill(0);
        self.ram_touched_bytes = 0;
        self.registers.fill(0);
        self.registers[15] = self.video_base as u32;
        self.pc = self.boot.address as u32;
        self.comparison = 0;
        self.disk_cursor = 0;
        self.status = if self.cycles >= self.config.max_cycles {
            "budget"
        } else {
            "ready"
        }
        .into();
        self.fault = None;
        self.serial.clear();
        self.serial_count = 0;
        self.keys.clear();
        let Boot {
            address,
            disk_offset,
            length,
        } = self.boot;
        self.ram[address..address + length]
            .copy_from_slice(&self.disk[disk_offset..disk_offset + length]);
        Self::mark(
            &mut self.ram_touched,
            &mut self.ram_touched_bytes,
            address,
            length,
        );
        Ok(())
    }
    pub fn read_memory(&self, address: usize, length: usize) -> Result<Vec<u8>, String> {
        range(address, length, self.ram.len(), "RAM")?;
        Ok(self.ram[address..address + length].to_vec())
    }
    pub fn read_disk(&self, address: usize, length: usize) -> Result<Vec<u8>, String> {
        range(address, length, self.disk.len(), "Disk")?;
        Ok(self.disk[address..address + length].to_vec())
    }
    pub fn write_memory(&mut self, address: usize, bytes: &[u8]) -> Result<(), String> {
        self.writable()?;
        self.store(address, bytes)
    }
    pub fn write_disk(&mut self, address: usize, bytes: &[u8]) -> Result<(), String> {
        self.writable()?;
        range(address, bytes.len(), self.disk.len(), "Disk")?;
        self.disk[address..address + bytes.len()].copy_from_slice(bytes);
        Self::mark(
            &mut self.disk_touched,
            &mut self.disk_touched_bytes,
            address,
            bytes.len(),
        );
        Ok(())
    }
    fn store(&mut self, address: usize, bytes: &[u8]) -> Result<(), String> {
        range(address, bytes.len(), self.ram.len(), "RAM")?;
        self.memory_changes
            .record(address, &self.ram[address..address + bytes.len()], bytes);
        self.ram[address..address + bytes.len()].copy_from_slice(bytes);
        Self::mark(
            &mut self.ram_touched,
            &mut self.ram_touched_bytes,
            address,
            bytes.len(),
        );
        Ok(())
    }
    /// Drain bounded changes since the previous call. Reads do not consume these
    /// events; the host drains once after each operation and fans out to observers.
    pub fn take_memory_changes(&mut self) -> MemoryChanges {
        std::mem::take(&mut self.memory_changes)
    }
    fn word(&self, address: usize) -> Result<u32, String> {
        range(address, 4, self.ram.len(), "RAM")?;
        Ok(u32::from_le_bytes(
            self.ram[address..address + 4].try_into().unwrap(),
        ))
    }
    pub fn input(&mut self, text: &str) -> Result<(), String> {
        if self
            .keys
            .len()
            .checked_add(text.len())
            .is_none_or(|n| n > 4096)
        {
            return Err("Keyboard queue is full (4096 bytes)".into());
        }
        self.keys.extend(text.bytes());
        Ok(())
    }
    fn push(&mut self, value: u32) -> Result<(), String> {
        let address = self.registers[15]
            .checked_sub(4)
            .ok_or("Stack underflow outside RAM")? as usize;
        self.store(address, &value.to_le_bytes())?;
        self.registers[15] = address as u32;
        Ok(())
    }
    fn pop(&mut self) -> Result<u32, String> {
        let address = self.registers[15] as usize;
        let value = self.word(address)?;
        self.registers[15] = (address + 4) as u32;
        Ok(value)
    }
    fn input_port(&mut self, port: u32) -> Result<u32, String> {
        match port {
            0 => Ok(0),
            1 => Ok(self.keys.pop_front().map(u32::from).unwrap_or(u32::MAX)),
            2 => Ok(self.disk_cursor as u32),
            3 => {
                range(self.disk_cursor, 1, self.disk.len(), "Disk")?;
                let byte = self.disk[self.disk_cursor];
                self.disk_cursor += 1;
                Ok(byte as u32)
            }
            4 => Ok(self.cycles as u32),
            _ => Err(format!("Unknown input port {port}")),
        }
    }
    fn output_port(&mut self, port: u32, value: u32) -> Result<(), String> {
        match port {
            0 => {
                self.serial.push(char::from(value as u8));
                self.serial_count += 1;
                if self.serial_count > 16384 {
                    let first_bytes = self.serial.chars().next().unwrap().len_utf8();
                    self.serial.drain(..first_bytes);
                    self.serial_count -= 1;
                }
                Ok(())
            }
            2 => {
                range(value as usize, 0, self.disk.len(), "Disk cursor")?;
                self.disk_cursor = value as usize;
                Ok(())
            }
            3 => {
                range(self.disk_cursor, 1, self.disk.len(), "Disk")?;
                self.disk[self.disk_cursor] = value as u8;
                Self::mark(
                    &mut self.disk_touched,
                    &mut self.disk_touched_bytes,
                    self.disk_cursor,
                    1,
                );
                self.disk_cursor += 1;
                Ok(())
            }
            _ => Err(format!("Unknown or read-only output port {port}")),
        }
    }
    fn execute(&mut self, allowance: u64) -> Result<bool, String> {
        let pc = self.pc as usize;
        range(pc, 8, self.ram.len(), "Instruction")?;
        let opcode = self.ram[pc];
        let a = self.ram[pc + 1] as usize;
        let mode = self.ram[pc + 2];
        if opcode >= 32 || a > 15 || mode > 1 || self.ram[pc + 3] != 0 {
            return Err(format!("Malformed instruction at {pc}"));
        }
        let raw = self.word(pc + 4)?;
        if mode == 1 && raw > 15 {
            return Err(format!("Invalid operand register {raw}"));
        }
        let cost = instruction_cost(opcode);
        if self.cycles + cost > self.config.max_cycles {
            self.status = "budget".into();
            return Ok(false);
        }
        if cost > allowance {
            return Ok(false);
        }
        let operand = if mode == 1 {
            self.registers[raw as usize]
        } else {
            raw
        };
        let value = self.registers[a];
        let mut next = self.pc + 8;
        self.cycles += cost;
        self.instructions += 1;
        match opcode {
            0 => (),
            1 => self.status = "halted".into(),
            2 => self.status = "paused".into(),
            3 => self.registers[a] = operand,
            4 => self.registers[a] = value.wrapping_add(operand),
            5 => self.registers[a] = value.wrapping_sub(operand),
            6 => self.registers[a] = value.wrapping_mul(operand),
            7 => self.registers[a] = value.checked_div(operand).ok_or("Division by zero")?,
            8 => self.registers[a] = value.checked_rem(operand).ok_or("Division by zero")?,
            9 => self.registers[a] = value & operand,
            10 => self.registers[a] = value | operand,
            11 => self.registers[a] = value ^ operand,
            12 => self.registers[a] = value.wrapping_shl(operand & 31),
            13 => self.registers[a] = value.wrapping_shr(operand & 31),
            14 => {
                range(operand as usize, 1, self.ram.len(), "RAM")?;
                self.registers[a] = self.ram[operand as usize] as u32;
            }
            15 => self.registers[a] = self.word(operand as usize)?,
            16 => self.store(operand as usize, &[value as u8])?,
            17 => self.store(operand as usize, &value.to_le_bytes())?,
            18 => {
                self.comparison = if value == operand {
                    0
                } else if value < operand {
                    -1
                } else {
                    1
                }
            }
            19 => next = operand,
            20 if self.comparison == 0 => next = operand,
            21 if self.comparison != 0 => next = operand,
            22 if self.comparison < 0 => next = operand,
            23 if self.comparison <= 0 => next = operand,
            24 if self.comparison > 0 => next = operand,
            25 if self.comparison >= 0 => next = operand,
            26 => {
                range(operand as usize, 8, self.ram.len(), "Call")?;
                self.push(next)?;
                next = operand;
            }
            27 => next = self.pop()?,
            28 => self.push(operand)?,
            29 => self.registers[a] = self.pop()?,
            30 => self.registers[a] = self.input_port(operand)?,
            31 => self.output_port(operand, value)?,
            _ => (),
        }
        self.pc = next;
        Ok(true)
    }
    pub fn run(&mut self, cycle_budget: u64) -> Result<Value, String> {
        bounded(cycle_budget, 1, 2_000_000, "cycleBudget")?;
        self.run_internal(cycle_budget, u64::MAX)
    }
    pub fn step(&mut self) -> Result<Value, String> {
        self.run_internal(16, 1)
    }
    fn run_internal(&mut self, budget: u64, mut count: u64) -> Result<Value, String> {
        if matches!(self.status.as_str(), "halted" | "fault" | "budget") {
            return Ok(self.state());
        }
        self.status = "ready".into();
        let start = self.cycles;
        while self.cycles - start < budget && count > 0 {
            count -= 1;
            match self.execute(budget - (self.cycles - start)) {
                Ok(false) => break,
                Err(error) => {
                    self.status = "fault".into();
                    self.fault = Some(error);
                    break;
                }
                Ok(true) => {
                    if self.status != "ready" {
                        break;
                    }
                }
            }
        }
        if self.status == "ready" {
            self.status = if self.cycles >= self.config.max_cycles {
                "budget"
            } else {
                "paused"
            }
            .into();
        }
        Ok(self.state())
    }
    /// A hardware limit reached by execution, including a remaining cycle
    /// allowance too small for the next instruction. External operation errors
    /// are classified separately with `resource_limit_error` so rejecting an
    /// invalid write/read does not mutate guest state or fabricate occupancy.
    pub fn resource_limit(&self) -> Option<&'static str> {
        if self.status == "budget" || self.cycles >= self.config.max_cycles {
            Some("cycles")
        } else {
            self.fault.as_deref().and_then(resource_limit_error)
        }
    }
    pub fn state(&self) -> Value {
        json!({ "config": self.config, "pc": self.pc, "registers": self.registers, "cycles": self.cycles,
            "instructions": self.instructions, "status": self.status, "fault": self.fault, "serial": self.serial,
            "videoBase": self.video_base, "bootBytes": self.boot.length, "sealed": self.sealed,
            "ramTouchedBytes": self.ram_touched_bytes, "diskTouchedBytes": self.disk_touched_bytes,
            "diskCursor": self.disk_cursor, "inputBytes": self.keys.len(),
            "virtualSeconds": self.cycles as f64 / self.config.clock_hz as f64 })
    }
    pub fn export_state(&self) -> Value {
        let mut snapshot = self.state();
        let obj = snapshot.as_object_mut().unwrap();
        obj.insert("version".into(), json!(1));
        obj.insert("comparison".into(), json!(self.comparison));
        obj.insert("keys".into(), json!(self.keys));
        obj.insert("boot".into(), json!(self.boot));
        for (key, bytes) in [
            ("ram", &self.ram),
            ("disk", &self.disk),
            ("ramTouched", &self.ram_touched),
            ("diskTouched", &self.disk_touched),
        ] {
            obj.insert(key.into(), json!(STANDARD.encode(bytes)));
        }
        snapshot
    }
    pub fn from_state(snapshot: &Value) -> Result<Self, String> {
        if snapshot.get("version").and_then(Value::as_u64) != Some(1) {
            return Err("Unsupported snapshot version".into());
        }
        let config: Config = serde_json::from_value(
            snapshot
                .get("config")
                .cloned()
                .ok_or("Missing snapshot config")?,
        )
        .map_err(|e| format!("Invalid snapshot config: {e}"))?;
        config.validate()?;
        let number = |key: &str, max: u64| -> Result<u64, String> {
            let value = snapshot
                .get(key)
                .and_then(Value::as_u64)
                .ok_or_else(|| format!("Invalid snapshot {key}"))?;
            bounded(value, 0, max, key)?;
            Ok(value)
        };
        let pc = number("pc", u32::MAX as u64)? as u32;
        let cycles = number("cycles", config.max_cycles)?;
        let instructions = number("instructions", cycles)?;
        let disk_cursor = number("diskCursor", config.disk_bytes as u64)? as usize;
        let registers: [u32; 16] = serde_json::from_value(
            snapshot
                .get("registers")
                .cloned()
                .ok_or("Missing snapshot registers")?,
        )
        .map_err(|_| "Invalid snapshot registers")?;
        let status = snapshot
            .get("status")
            .and_then(Value::as_str)
            .filter(|s| matches!(*s, "ready" | "paused" | "halted" | "fault" | "budget"))
            .ok_or("Invalid snapshot status")?
            .to_owned();
        let fault = match snapshot.get("fault") {
            Some(Value::Null) => None,
            Some(Value::String(s)) if s.len() <= 1024 => Some(s.clone()),
            _ => return Err("Invalid snapshot fault".into()),
        };
        let serial = snapshot
            .get("serial")
            .and_then(Value::as_str)
            .filter(|s| s.chars().count() <= 16384 && s.chars().all(|c| (c as u32) <= 255))
            .ok_or("Invalid snapshot serial")?
            .to_owned();
        let sealed = snapshot
            .get("sealed")
            .and_then(Value::as_bool)
            .ok_or("Invalid snapshot seal")?;
        let comparison = snapshot
            .get("comparison")
            .and_then(Value::as_i64)
            .filter(|n| (-1..=1).contains(n))
            .ok_or("Invalid snapshot comparison")? as i8;
        let keys: VecDeque<u8> = serde_json::from_value(
            snapshot
                .get("keys")
                .cloned()
                .ok_or("Missing snapshot input")?,
        )
        .map_err(|_| "Invalid snapshot input")?;
        if keys.len() > 4096 {
            return Err("Invalid snapshot input size".into());
        }
        let boot: Boot = serde_json::from_value(
            snapshot
                .get("boot")
                .cloned()
                .ok_or("Missing snapshot boot")?,
        )
        .map_err(|_| "Invalid snapshot boot")?;
        range(
            boot.address,
            boot.length,
            config.ram_bytes - VIDEO_BYTES,
            "Snapshot boot RAM",
        )?;
        range(
            boot.disk_offset,
            boot.length,
            config.disk_bytes,
            "Snapshot boot disk",
        )?;
        let decode = |key: &str, length: usize| -> Result<Vec<u8>, String> {
            let text = snapshot
                .get(key)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("Invalid snapshot {key}"))?;
            if text.len() != length.div_ceil(3) * 4 {
                return Err(format!("Invalid snapshot {key} size"));
            }
            let bytes = STANDARD
                .decode(text)
                .map_err(|_| format!("Invalid snapshot {key} encoding"))?;
            if bytes.len() != length || STANDARD.encode(&bytes) != text {
                return Err(format!("Invalid snapshot {key} size/encoding"));
            }
            Ok(bytes)
        };
        let ram = decode("ram", config.ram_bytes)?;
        let disk = decode("disk", config.disk_bytes)?;
        let ram_touched = decode("ramTouched", config.ram_bytes.div_ceil(8))?;
        let disk_touched = decode("diskTouched", config.disk_bytes.div_ceil(8))?;
        let count = |bitmap: &[u8], capacity: usize| -> Result<usize, String> {
            if capacity % 8 != 0
                && bitmap
                    .last()
                    .is_some_and(|byte| byte >> (capacity % 8) != 0)
            {
                return Err("Invalid snapshot touched bitmap padding".into());
            }
            Ok(bitmap.iter().map(|b| b.count_ones() as usize).sum())
        };
        let ram_touched_bytes = count(&ram_touched, config.ram_bytes)?;
        let disk_touched_bytes = count(&disk_touched, config.disk_bytes)?;
        let serial_count = serial.chars().count();
        Ok(Self {
            video_base: config.ram_bytes - VIDEO_BYTES,
            config,
            ram,
            disk,
            registers,
            pc,
            cycles,
            instructions,
            status,
            fault,
            serial,
            sealed,
            ram_touched_bytes,
            disk_touched_bytes,
            disk_cursor,
            ram_touched,
            disk_touched,
            comparison,
            keys,
            boot,
            serial_count,
            memory_changes: MemoryChanges::default(),
        })
    }
}

#[cfg(test)]
mod memory_change_tests {
    use super::*;

    fn machine() -> Machine {
        Machine::new(Config {
            ram_bytes: 65536,
            disk_bytes: 4096,
            clock_hz: 1000,
            max_cycles: 1000,
        })
        .unwrap()
    }

    #[test]
    fn tracks_only_changes_and_preserves_write_then_restore() {
        let mut vm = machine();
        assert_eq!(vm.take_memory_changes().changed_bytes, 0);
        vm.write_memory(100, &[0, 4, 5, 0, 7]).unwrap();
        vm.write_memory(100, &[0, 4, 5, 0, 7]).unwrap();
        vm.write_memory(101, &[0, 0]).unwrap();
        let changes = vm.take_memory_changes();
        assert_eq!(changes.changed_bytes, 5);
        assert_eq!(changes.ranges.len(), 2);
        assert_eq!(changes.ranges[0].address, 101);
        assert_eq!(changes.ranges[0].changed_bytes, 3);
        assert_eq!(changes.ranges[0].before, [0, 0, 0, 0]);
        assert_eq!(changes.ranges[0].after, [4, 5, 0, 7]);
        assert_eq!(changes.ranges[1].before, [4, 5]);
        assert_eq!(changes.ranges[1].after, [0, 0]);
        assert_eq!(vm.take_memory_changes().changed_bytes, 0);
    }

    #[test]
    fn coalesces_adjacent_writes_and_bounds_previews() {
        let mut vm = machine();
        for address in 100..200 {
            vm.write_memory(address, &[3]).unwrap();
        }
        let changes = vm.take_memory_changes();
        assert_eq!(changes.ranges.len(), 1);
        assert_eq!(changes.changed_bytes, 100);
        assert_eq!(changes.ranges[0].length, 100);
        assert_eq!(changes.ranges[0].before, [0; 32]);
        assert_eq!(changes.ranges[0].after, [3; 32]);
        assert!(!changes.truncated);
    }

    #[test]
    fn range_limit_keeps_total_count_and_signals_truncation() {
        let mut vm = machine();
        for address in (100..300).step_by(2) {
            vm.write_memory(address, &[1]).unwrap();
        }
        let changes = vm.take_memory_changes();
        assert_eq!(changes.changed_bytes, 100);
        assert_eq!(changes.ranges.len(), 64);
        assert!(changes.truncated);
        assert_eq!(changes.ranges.last().unwrap().address, 226);
    }

    #[test]
    fn tracks_load_reset_and_guest_store_but_not_restore() {
        let mut vm = machine();
        let program =
            crate::assembler::assemble("mov r0, 1\nstore8 r0, 0\nhalt\n", &vm.config).unwrap();
        vm.load(&program.bytes).unwrap();
        assert!(vm.take_memory_changes().changed_bytes > 0);
        vm.run(100).unwrap();
        let changes = vm.take_memory_changes();
        assert_eq!(changes.changed_bytes, 1);
        assert_eq!(changes.ranges[0].address, 0);
        assert_eq!(changes.ranges[0].before, [3]);
        assert_eq!(changes.ranges[0].after, [1]);
        let mut restored = Machine::from_state(&vm.export_state()).unwrap();
        assert_eq!(restored.take_memory_changes().changed_bytes, 0);
        vm.reset().unwrap();
        let changes = vm.take_memory_changes();
        assert!(changes.changed_bytes > 0);
        assert_eq!(vm.ram[0], 3);
    }

    #[test]
    fn reassembly_reports_final_bytes_and_groups_instruction_padding() {
        let mut vm = machine();
        let old = crate::assembler::assemble(&"mov r0, 1\n".repeat(100), &vm.config).unwrap();
        let new = crate::assembler::assemble(&"mov r0, 2\n".repeat(100), &vm.config).unwrap();
        vm.load(&old.bytes).unwrap();
        vm.take_memory_changes();
        vm.load(&new.bytes).unwrap();
        let changes = vm.take_memory_changes();
        assert_eq!(changes.changed_bytes, 100);
        assert_eq!(changes.ranges.len(), 1);
        assert!(!changes.truncated);
        let range = &changes.ranges[0];
        assert_eq!(range.address, 4);
        assert_eq!(range.changed_bytes, 100);
        assert_eq!(range.before, old.bytes[4..36]);
        assert_eq!(range.after, new.bytes[4..36]);
        vm.reset().unwrap();
        assert_eq!(vm.take_memory_changes().changed_bytes, 0);
    }

    #[test]
    fn boot_at_an_address_clears_other_ram_atomically() {
        let mut vm = machine();
        vm.write_memory(20, &[9]).unwrap();
        vm.write_memory(300, &[9]).unwrap();
        vm.take_memory_changes();
        vm.load_at(&[1, 0, 2, 0], 100, 20).unwrap();
        let changes = vm.take_memory_changes();
        assert_eq!(changes.changed_bytes, 4);
        assert_eq!(changes.ranges.len(), 3);
        assert_eq!(changes.ranges[0].address, 20);
        assert_eq!(changes.ranges[0].after, [0]);
        assert_eq!(changes.ranges[1].address, 100);
        assert_eq!(changes.ranges[1].after, [1, 0, 2]);
        assert_eq!(changes.ranges[2].address, 300);
        assert_eq!(changes.ranges[2].after, [0]);
    }
}
