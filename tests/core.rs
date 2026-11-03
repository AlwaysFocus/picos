use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::json;
use smol_os::assembler::assemble;
use smol_os::vm::{Config, Machine, VIDEO_BYTES};

fn config() -> Config {
    Config {
        ram_bytes: 65536,
        disk_bytes: 32768,
        clock_hz: 300_000_000,
        max_cycles: 2_000_000,
    }
}
fn machine(source: &str) -> Machine {
    machine_config(source, config())
}
fn machine_config(source: &str, config: Config) -> Machine {
    let bytes = assemble(source, &config).unwrap().bytes;
    let mut vm = Machine::new(config).unwrap();
    vm.load(&bytes).unwrap();
    vm
}

#[test]
fn assembler_resolves_labels_constants_literals_and_utf8_data() {
    let source = ".equ VALUE, end - start\nstart: mov r0, VALUE\njmp end\n.ascii \"a;,//b\" ; comment\n.byte 0xff, 'x'\n.word -1\nend: halt";
    let asm = assemble(source, &config()).unwrap();
    assert_eq!(asm.symbols["VALUE"], asm.symbols["end"]);
    assert_eq!(
        u32::from_le_bytes(asm.bytes[4..8].try_into().unwrap()),
        asm.symbols["end"]
    );
    assert_eq!(asm.bytes[asm.symbols["end"] as usize], 1);
    assert_eq!(asm.listing[0]["address"], 0);
    let utf = assemble(".asciz \"é\"", &config()).unwrap();
    assert_eq!(utf.bytes, [195, 169, 0]);
    assert_eq!(utf.symbols["VIDEO_BASE"], 16384);
}

#[test]
fn assembler_rejects_invalid_code_and_host_expressions() {
    for source in [
        "mov r0, process.exit()",
        "nope r0",
        "mov r16, 1",
        "mov r0, r99",
        "mov r0, r00",
        "mov r0, missing",
        "same: nop\nsame: halt",
        ".byte 256",
        ".space -1",
        ".equ X, Y\n.equ Y, X\nmov r0, X",
        ".ascii \"bad",
        "halt r0",
        "mov r0, 4294967296",
        "mov r0, 1 +",
        "mov r0, 1 2",
    ] {
        assert!(
            assemble(source, &config()).is_err(),
            "Unexpectedly accepted {source}"
        );
    }
    assert!(
        assemble("nop\nmov r0, missing", &config())
            .unwrap_err()
            .contains("Line 2")
    );
    let nested = format!("mov r0, {}1{}", "(".repeat(256), ")".repeat(256));
    assert!(assemble(&nested, &config()).is_err());
}

#[test]
fn assembler_bounds_image_before_allocating() {
    assert!(
        assemble(".space 20000", &config())
            .unwrap_err()
            .contains("capacity")
    );
    assert!(assemble(".space 9223372036854775807", &config()).is_err());
    assert!(
        assemble(
            "nop\nhalt",
            &Config {
                disk_bytes: 8,
                ..config()
            }
        )
        .is_err()
    );
}

#[test]
fn unsigned_arithmetic_wraps_and_keeps_low_product_bits() {
    let mut vm = machine(
        "mov r0, 0xffffffff\nadd r0, 1\nsub r0, 1\nmov r1, 0xffffffff\nmul r1, 0xffffffff\nmov r2, 0xffffffff\ndiv r2, 2\nshr r0, 31\nhalt",
    );
    vm.run(1000).unwrap();
    assert_eq!(vm.registers[0], 1);
    assert_eq!(vm.registers[1], 1);
    assert_eq!(vm.registers[2], 2147483647);
    assert_eq!(vm.status, "halted");
}

#[test]
fn arithmetic_matches_u32_reference_across_boundary_values() {
    let values = [
        0u32,
        1,
        31,
        32,
        255,
        65535,
        2147483647,
        2147483648,
        u32::MAX,
    ];
    for left in values {
        for right in values {
            let mut vm = machine(&format!(
                "mov r0, {left}\nadd r0, {right}\nmov r1, {left}\nsub r1, {right}\nmov r2, {left}\nmul r2, {right}\nmov r3, {left}\nshr r3, {right}\nhalt"
            ));
            vm.run(100).unwrap();
            assert_eq!(vm.registers[0], left.wrapping_add(right));
            assert_eq!(vm.registers[1], left.wrapping_sub(right));
            assert_eq!(vm.registers[2], left.wrapping_mul(right));
            assert_eq!(vm.registers[3], left.wrapping_shr(right & 31));
        }
    }
}

#[test]
fn calls_stack_and_unsigned_conditional_branches_work() {
    let mut vm = machine(
        "mov r0, 0xffffffff\ncmp r0, 1\njgt higher\nhalt\nhigher: call function\npop r2\nhalt\nfunction: pop r3\npush 42\npush r3\nret",
    );
    vm.run(1000).unwrap();
    assert_eq!(vm.registers[2], 42);
    assert_eq!(vm.registers[15], vm.video_base as u32);
}

#[test]
fn slice_and_lifetime_budgets_require_complete_instruction_costs() {
    let mut vm = machine_config(
        "mov r0, 9\nmul r0, 2\nhalt",
        Config {
            max_cycles: 5,
            ..config()
        },
    );
    vm.run(3).unwrap();
    assert_eq!(vm.pc, 8);
    assert_eq!(vm.cycles, 1);
    vm.run(3).unwrap();
    assert_eq!(vm.cycles, 1);
    vm.step().unwrap();
    assert_eq!(vm.registers[0], 18);
    assert_eq!(vm.cycles, 5);
    assert_eq!(vm.status, "budget");
    vm.reset().unwrap();
    assert_eq!(vm.cycles, 5);
    assert_eq!(vm.status, "budget");
    assert!(vm.run(2_000_001).is_err());
    assert!(vm.run(0).is_err());
    let mut short = machine_config(
        "mov r0, 1\nmul r0, 1",
        Config {
            max_cycles: 4,
            ..config()
        },
    );
    short.run(100).unwrap();
    assert_eq!(short.cycles, 1);
    assert_eq!(short.status, "budget");
}

#[test]
fn unsafe_memory_malformed_code_and_divide_by_zero_fault() {
    for source in [
        "load32 r0, RAM_BYTES - 2",
        "store8 r0, RAM_BYTES",
        "mov r15, 0\npush 1",
        "mov r0, 1\ndiv r0, 0",
        ".byte 255\n.space 7",
        "jmp RAM_BYTES",
        "mov r0, 1\nmod r0, 0",
        "jmp 0xffffffff",
    ] {
        let mut vm = machine(source);
        vm.run(1000).unwrap();
        assert_eq!(vm.status, "fault", "{source}");
        assert!(vm.fault.is_some());
    }
    for offset in [1usize, 2, 3] {
        let mut vm = machine("nop");
        vm.write_memory(offset, &[255]).unwrap();
        vm.run(100).unwrap();
        assert_eq!(vm.status, "fault");
    }
}

#[test]
fn guest_generates_and_executes_instructions_in_mutable_ram() {
    let mut vm = machine(
        "mov r0, 3\nstore8 r0, 1024\nmov r0, 123\nstore32 r0, 1028\nmov r0, 1\nstore8 r0, 1032\njmp 1024",
    );
    vm.run(1000).unwrap();
    assert_eq!(vm.status, "halted");
    assert_eq!(vm.registers[0], 123);
    assert_eq!(vm.pc, 1040);
}

#[test]
fn keyboard_serial_and_raw_disk_work_and_disk_survives_reboot() {
    let mut vm = machine(
        "in r0, 1\nout r0, 0\nmov r1, 2048\nout r1, 2\nout r0, 3\nout r1, 2\nin r2, 3\nin r3, 1\nhalt",
    );
    vm.input("A").unwrap();
    vm.run(1000).unwrap();
    assert_eq!(vm.serial, "A");
    assert_eq!(vm.registers[2], 65);
    assert_eq!(vm.registers[3], u32::MAX);
    let consumed = vm.cycles;
    vm.reset().unwrap();
    assert_eq!(vm.disk[2048], 65);
    assert_eq!(vm.cycles, consumed);
    assert_eq!(vm.serial, "");
    assert!(vm.input(&"x".repeat(4097)).is_err());
    assert_eq!(vm.state()["inputBytes"], 0);
}

#[test]
fn disk_and_port_bounds_fault() {
    for source in [
        "mov r0, DISK_BYTES\nout r0, 2\nin r1, 3",
        "out r0, 4",
        "in r0, 99",
        "mov r0, DISK_BYTES + 1\nout r0, 2",
    ] {
        let mut vm = machine(source);
        vm.run(1000).unwrap();
        assert_eq!(vm.status, "fault");
    }
}

#[test]
fn yield_and_single_step_resume_at_next_instruction() {
    let mut vm = machine("mov r0, 1\nyield\nadd r0, 2\nhalt");
    vm.run(100).unwrap();
    assert_eq!(vm.registers[0], 1);
    assert_eq!(vm.status, "paused");
    assert_eq!(vm.pc, 16);
    vm.step().unwrap();
    assert_eq!(vm.registers[0], 3);
    assert_eq!(vm.pc, 24);
    vm.run(100).unwrap();
    assert_eq!(vm.status, "halted");
}

#[test]
fn sealing_denies_injection_but_allows_guest_stores_and_disk_io() {
    let mut vm = machine("mov r0, 7\nstore8 r0, VIDEO_BASE\nout r0, 3\nhalt");
    vm.sealed = true;
    assert!(vm.write_memory(0, &[1]).is_err());
    assert!(vm.write_disk(0, &[1]).is_err());
    assert!(vm.load(&[1]).is_err());
    vm.run(100).unwrap();
    assert_eq!(vm.ram[vm.video_base], 7);
    assert_eq!(vm.disk[0], 7);
}

#[test]
fn snapshot_roundtrip_preserves_cpu_ram_disk_input_flags_seal_and_accounting() {
    let mut vm = machine("mov r0, 7\nstore8 r0, VIDEO_BASE\ncmp r0, 8\nyield\nin r1, 1\nhalt");
    vm.input("z").unwrap();
    vm.run(100).unwrap();
    vm.sealed = true;
    let json = serde_json::to_string(&vm.export_state()).unwrap();
    let mut restored = Machine::from_state(&serde_json::from_str(&json).unwrap()).unwrap();
    assert_eq!(restored.state(), vm.state());
    assert_eq!(restored.ram, vm.ram);
    assert_eq!(restored.disk, vm.disk);
    restored.run(100).unwrap();
    vm.run(100).unwrap();
    assert_eq!(restored.state(), vm.state());
}

#[test]
fn malformed_snapshots_are_rejected_without_panics_or_capacity_changes() {
    let vm = machine("halt");
    for (key, bad) in [
        ("ram", json!("AAAA")),
        ("disk", json!("!!!!")),
        ("registers", json!([0])),
        ("pc", json!(-1)),
        ("cycles", json!(u64::MAX)),
        ("instructions", json!(100)),
        ("sealed", json!("false")),
        ("comparison", json!(2)),
        ("keys", json!([256])),
        ("status", json!("running")),
        (
            "boot",
            json!({"address":u64::MAX,"length":8,"diskOffset":0}),
        ),
        ("serial", json!("💻")),
    ] {
        let mut snapshot = vm.export_state();
        snapshot[key] = bad;
        assert!(
            Machine::from_state(&snapshot).is_err(),
            "Accepted invalid {key}"
        );
    }
    let mut snapshot = vm.export_state();
    snapshot["config"]["ramBytes"] = json!(u64::MAX);
    assert!(Machine::from_state(&snapshot).is_err());
    let odd = Machine::new(Config {
        ram_bytes: 65537,
        disk_bytes: 9,
        ..config()
    })
    .unwrap();
    let mut snapshot = odd.export_state();
    snapshot["diskTouched"] = json!(STANDARD.encode([0, 128]));
    assert!(Machine::from_state(&snapshot).is_err());
}

#[test]
fn external_memory_and_config_boundaries_are_atomic_and_overflow_safe() {
    assert!(
        Machine::new(Config {
            ram_bytes: 0,
            ..config()
        })
        .is_err()
    );
    assert!(
        Machine::new(Config {
            disk_bytes: usize::MAX,
            ..config()
        })
        .is_err()
    );
    assert!(
        Machine::new(Config {
            max_cycles: 0,
            ..config()
        })
        .is_err()
    );
    let mut vm = machine("halt");
    for address in [vm.ram.len(), vm.ram.len() + 1, usize::MAX] {
        assert!(vm.write_memory(address, &[1]).is_err());
        assert!(vm.read_memory(address, 1).is_err());
    }
    assert!(vm.read_memory(1, usize::MAX).is_err());
    assert!(vm.write_disk(usize::MAX, &[1]).is_err());
    assert!(vm.load_at(&[1], usize::MAX, 0).is_err());
    assert_eq!(vm.ram[0], 1);
    assert_eq!(vm.disk[0], 1);
}

#[test]
fn signal_example_draws_guest_framebuffer_and_serial_within_budget() {
    let mut vm = machine(include_str!("../examples/signal.asm"));
    vm.run(2_000_000).unwrap();
    assert_eq!(vm.status, "halted");
    assert!(vm.serial.contains("signal acquired"));
    assert!(vm.ram[vm.video_base..].iter().any(|b| *b != 0));
    assert!(vm.ram_touched_bytes >= VIDEO_BYTES);
    assert_eq!(vm.cycles, 837592);
}

#[test]
fn echo_example_waits_and_resumes_for_successive_keyboard_input() {
    let mut vm = machine(include_str!("../examples/echo.asm"));
    vm.run(10000).unwrap();
    assert_eq!(vm.status, "paused");
    assert_eq!(vm.serial, "");
    vm.input("Hi").unwrap();
    vm.run(10000).unwrap();
    assert_eq!(vm.serial, "Hi");
    assert_eq!(&vm.ram[vm.video_base..vm.video_base + 2], &[72, 105]);
    assert_eq!(vm.state()["inputBytes"], 0);
    vm.input("!").unwrap();
    vm.run(10000).unwrap();
    assert_eq!(vm.serial, "Hi!");
    assert_eq!(vm.ram[vm.video_base + 2], 33);
}

#[test]
fn resource_classification_distinguishes_hardware_boundaries_from_guest_bugs() {
    use smol_os::vm::resource_limit_error;
    for (source, resource) in [
        ("load8 r0, RAM_BYTES", Some("ram")),
        ("store32 r0, RAM_BYTES - 1", Some("ram")),
        ("mov r15, 0\npush 1", Some("ram")),
        ("jmp RAM_BYTES", Some("ram")),
        ("call RAM_BYTES", Some("ram")),
        ("mov r0, DISK_BYTES\nout r0, 2\nin r1, 3", Some("disk")),
        ("mov r0, DISK_BYTES + 1\nout r0, 2", Some("disk")),
        ("div r0, 0", None),
        ("in r0, 999", None),
        (".byte 255\n.space 7", None),
    ] {
        let mut vm = machine(source);
        vm.run(100).unwrap();
        assert_eq!(vm.resource_limit(), resource, "{source}");
        let restored = Machine::from_state(&vm.export_state()).unwrap();
        assert_eq!(restored.resource_limit(), resource, "restored {source}");
    }
    let mut vm = machine("halt");
    let before = vm.export_state();
    for error in [
        vm.write_memory(65536, &[1]).unwrap_err(),
        vm.read_memory(65536, 1).unwrap_err(),
        vm.load_at(&[1; 8], vm.video_base, 0).unwrap_err(),
    ] {
        assert_eq!(resource_limit_error(&error), Some("ram"));
    }
    for error in [
        vm.write_disk(32768, &[1]).unwrap_err(),
        vm.read_disk(32768, 1).unwrap_err(),
    ] {
        assert_eq!(resource_limit_error(&error), Some("disk"));
    }
    assert_eq!(
        vm.export_state(),
        before,
        "rejected external operations stay atomic"
    );
    assert_eq!(vm.resource_limit(), None);
    let error = assemble(".space RAM_BYTES", &config()).unwrap_err();
    assert_eq!(resource_limit_error(&error), Some("boot"));
    for error in [
        "Division by zero",
        "Assembly source exceeds 1 MiB",
        "Keyboard queue is full (4096 bytes)",
        "not RAM access outside 10 bytes: 11 + 1",
        "RAM access outside nope bytes: 11 + 1",
        "Line nope: Image exceeds available boot capacity (10 bytes)",
    ] {
        assert_eq!(resource_limit_error(error), None, "{error}");
    }
}

#[test]
fn cycle_limit_is_reached_when_next_instruction_cannot_fit() {
    let mut limits = config();
    limits.max_cycles = 5;
    let mut vm = machine_config("mov r0, 1\nout r0, 0", limits);
    vm.run(100).unwrap();
    assert_eq!(vm.cycles, 1);
    assert_eq!(vm.resource_limit(), Some("cycles"));
    let restored = Machine::from_state(&vm.export_state()).unwrap();
    assert_eq!(restored.resource_limit(), Some("cycles"));
}
