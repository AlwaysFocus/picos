use smol_os::{
    assembler::assemble,
    disassembler::disassemble,
    vm::{Config, MAX_DISK_BYTES, MAX_RAM_BYTES, Machine},
};

fn machine(source: &str) -> Machine {
    let config = Config {
        ram_bytes: 65536,
        disk_bytes: 4096,
        ..Config::default()
    };
    let image = assemble(source, &config).unwrap();
    let mut machine = Machine::new(config).unwrap();
    machine.load(&image.bytes).unwrap();
    machine
}

#[test]
fn decodes_registers_immediates_branches_stack_and_pc_from_live_ram() {
    let mut vm = machine(
        "mov r2, 42\nadd r2, r3\nload8 r4, 256\nstore32 r4, r5\njmp 64\ncall r8\npush 77\npop r9\nret\nhalt",
    );
    vm.pc = 16;
    vm.registers[2] = 123;
    let view = disassemble(&vm, 0, 80).unwrap();
    let instructions: Vec<_> = view["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["assembly"].as_str().unwrap())
        .collect();
    assert_eq!(
        instructions,
        [
            "mov r2, 42",
            "add r2, r3",
            "load8 r4, 0x00000100",
            "store32 r4, r5",
            "jmp 0x00000040",
            "call r8",
            "push 77",
            "pop r9",
            "ret",
            "halt",
        ]
    );
    assert_eq!(view["rows"][2]["isPc"], true);
    assert_eq!(view["rows"][1]["isPc"], false);
    assert_eq!(view["registers"][2], 123);
    assert_eq!(view["rows"][0]["bytes"][0], 3);
    assert_eq!(view["ramBytes"], 65536);
    assert_eq!(view["videoBase"], 16384);
}

#[test]
fn guest_self_modification_changes_disassembly_without_reassembly() {
    let mut vm = machine("mov r0, 1\nstore8 r0, target\nyield\ntarget: nop\nhalt");
    assert_eq!(
        disassemble(&vm, 24, 8).unwrap()["rows"][0]["assembly"],
        "nop"
    );
    vm.run(100).unwrap();
    let changed = disassemble(&vm, 24, 8).unwrap();
    assert_eq!(vm.status, "paused");
    assert_eq!(changed["rows"][0]["assembly"], "halt");
    assert_eq!(changed["rows"][0]["bytes"][0], 1);
    assert_eq!(changed["rows"][0]["isPc"], true);
    // The persistent initial boot image has not changed: the view is RAM only.
    assert_eq!(vm.read_disk(24, 1).unwrap(), [0]);
}

#[test]
fn malformed_encodings_and_partial_rows_are_explicit_data() {
    let mut vm = machine("halt");
    for bad in [
        [32, 0, 0, 0, 0, 0, 0, 0],
        [3, 16, 0, 0, 0, 0, 0, 0],
        [3, 0, 2, 0, 0, 0, 0, 0],
        [3, 0, 0, 1, 0, 0, 0, 0],
        [3, 0, 1, 0, 16, 0, 0, 0],
        [1, 0, 1, 0, 16, 0, 0, 0],
    ] {
        vm.write_memory(0, &bad).unwrap();
        let view = disassemble(&vm, 0, 8).unwrap();
        assert_eq!(view["rows"][0]["kind"], "data");
        assert!(
            view["rows"][0]["assembly"]
                .as_str()
                .unwrap()
                .starts_with(".byte ")
        );
        assert_eq!(view["rows"][0]["bytes"], serde_json::json!(bad));
    }
    vm.write_memory(0, &[1, 0, 0, 0, 0, 0, 0, 0, 0xab, 0xcd])
        .unwrap();
    let view = disassemble(&vm, 0, 10).unwrap();
    assert_eq!(view["rows"][0]["assembly"], "halt");
    assert_eq!(view["rows"][1]["assembly"], ".byte 0xab, 0xcd");
    assert_eq!(view["rows"][1]["address"], 8);
}

#[test]
fn instruction_alignment_is_selected_by_the_observer_not_assumed() {
    let mut vm = machine(".byte 255\nmov r7, 12\nhalt");
    vm.pc = 1;
    let view = disassemble(&vm, 1, 16).unwrap();
    assert_eq!(view["address"], 1);
    assert_eq!(view["rows"][0]["assembly"], "mov r7, 12");
    assert_eq!(view["rows"][0]["isPc"], true);
    assert_eq!(view["rows"][1]["address"], 9);
    assert_eq!(disassemble(&vm, 0, 8).unwrap()["rows"][0]["kind"], "data");
}

#[test]
fn decoder_accepts_exactly_the_cpu_header_rules_even_for_ignored_fields() {
    let mut vm = machine("halt");
    // Halt ignores the valid destination and operand after header validation.
    vm.write_memory(0, &[1, 15, 1, 0, 15, 0, 0, 0]).unwrap();
    assert_eq!(
        disassemble(&vm, 0, 8).unwrap()["rows"][0]["assembly"],
        "halt"
    );
    vm.run(20).unwrap();
    assert_eq!(vm.status, "halted");
}

#[test]
fn inspection_is_bounded_and_does_not_execute_or_mutate_the_guest() {
    let vm = machine("mov r0, 999\nhalt");
    for (address, length) in [(0, 0), (0, 7), (0, 4097), (65532, 8), (usize::MAX, 8)] {
        assert!(disassemble(&vm, address, length).is_err());
    }
    assert!(disassemble(&vm, 0, 4096).is_ok());
    assert!(disassemble(&vm, 65528, 8).is_ok());
    assert_eq!(vm.pc, 0);
    assert_eq!(vm.cycles, 0);
    assert_eq!(vm.registers[0], 0);
}

#[test]
fn requested_decimal_megabyte_profile_and_capacity_boundaries_validate() {
    let mut config = Config {
        ram_bytes: 24_000_000,
        disk_bytes: 200_000_000,
        clock_hz: 300_000_000,
        ..Config::default()
    };
    assert!(config.validate().is_ok());
    config.ram_bytes = MAX_RAM_BYTES;
    config.disk_bytes = MAX_DISK_BYTES;
    assert!(config.validate().is_ok());
    config.disk_bytes += 1;
    assert!(config.validate().is_err());
    config.disk_bytes = 200_000_000;
    config.ram_bytes += 1;
    assert!(config.validate().is_err());
}
