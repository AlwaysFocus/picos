//! After the one-time bootstrap load, the host only delivers text and clocks
//! the CPU. Parsing, code emission, execution, formatting and persistence are
//! all implemented by the sealed guest; expected answers below are constants.
use smol_os::assembler::assemble;
use smol_os::vm::{Config, Machine, resource_limit_error};

const SOURCE: &str = include_str!("../examples/sealed-interpreter.asm");
const CODE: usize = 4096;
const RECORD: usize = 4096;
const MAX_CODE: usize = 136;

fn config() -> Config {
    Config {
        ram_bytes: 65536,
        disk_bytes: 8192,
        clock_hz: 300_000_000,
        max_cycles: 100_000,
    }
}

fn sealed(source: &str, config: Config) -> Machine {
    let image = assemble(source, &config).unwrap();
    assert!(
        image.bytes.len() < CODE,
        "bootstrap must not overlap scratch"
    );
    let mut vm = Machine::new(config).unwrap();
    vm.load(&image.bytes).unwrap();
    vm.sealed = true;
    vm
}

fn run_input(vm: &mut Machine, input: &str) -> u64 {
    let before = vm.cycles;
    vm.input(input).unwrap();
    assert_eq!(vm.cycles, before, "delivery is not guest computation");
    vm.run(100_000).unwrap();
    assert!(
        vm.cycles > before,
        "the helper must execute guest instructions"
    );
    assert_eq!(vm.fault, None);
    assert_eq!(vm.status, "paused");
    assert_eq!(vm.state()["inputBytes"], 0);
    vm.cycles - before
}

#[test]
fn sealed_guest_compiles_fresh_input_executes_it_and_recovers_from_disk() {
    let mut vm = sealed(SOURCE, config());
    assert!(vm.ram[CODE..CODE + MAX_CODE].iter().all(|byte| *byte == 0));
    let original = vm.export_state();
    assert!(vm.load(&[1; 8]).is_err());
    assert!(vm.write_memory(CODE, &[1; 8]).is_err());
    assert!(vm.write_disk(RECORD, &[1; 8]).is_err());
    assert_eq!(vm.export_state(), original);

    // Neither this expression nor its result is embedded in the boot program.
    let compilation_cycles = run_input(&mut vm, "12+30*2\n");
    assert_eq!(vm.serial, "=84\n");
    assert_eq!(vm.registers[12], 84);
    let native_code = [
        3, 0, 0, 0, 12, 0, 0, 0, // mov r0, 12
        4, 0, 0, 0, 30, 0, 0, 0, // add r0, 30
        6, 0, 0, 0, 2, 0, 0, 0, // mul r0, 2
        27, 0, 0, 0, 0, 0, 0, 0, // ret
    ];
    assert_eq!(&vm.ram[CODE..CODE + native_code.len()], &native_code);
    assert_eq!(&vm.disk[RECORD..RECORD + 6], &[b'S', 32, 84, 0, 0, 0]);
    assert_eq!(&vm.disk[RECORD + 6..RECORD + 38], &native_code);
    assert!(vm.cycles <= vm.config.max_cycles);
    assert_eq!(vm.ram.len(), 65536);
    assert_eq!(vm.disk.len(), 8192);
    assert!(vm.ram[vm.video_base..].iter().all(|byte| *byte == 0));

    let consumed = vm.cycles;
    vm.reset().unwrap();
    assert!(vm.sealed);
    assert_eq!(vm.cycles, consumed, "reboot must not refund cycles");
    assert!(vm.ram[CODE..CODE + MAX_CODE].iter().all(|byte| *byte == 0));
    vm.run(100_000).unwrap();
    assert_eq!(vm.fault, None);
    assert_eq!(vm.serial, "S=84\nR=84\n");
    assert_eq!(&vm.ram[CODE..CODE + native_code.len()], &native_code);
    assert!(vm.cycles > consumed, "reload and replay spend guest cycles");

    let replay_cycles = vm.cycles - consumed;
    run_input(&mut vm, "7*8+9\n");
    assert_eq!(vm.serial, "S=84\nR=84\n=65\n");
    assert_eq!(&vm.disk[RECORD + 2..RECORD + 6], &[65, 0, 0, 0]);
    eprintln!(
        "sealed guest: bootstrap={} B, generated={} B, compile/run/save={} cycles, reload/replay={} cycles",
        vm.state()["bootBytes"],
        native_code.len(),
        compilation_cycles,
        replay_cycles
    );
}

#[test]
fn guest_parser_yields_for_partial_input_and_rejects_invalid_lines() {
    let mut vm = sealed(SOURCE, config());
    run_input(&mut vm, "12+");
    assert_eq!(vm.serial, "");
    assert_eq!(vm.disk[RECORD], 0);
    run_input(&mut vm, "30\n");
    assert_eq!(vm.serial, "=42\n");
    let saved_disk = vm.disk.clone();
    for bad in ["\n", "12+\n", "-1\n", "1/2\n", "65536\n", "1 +2\n"] {
        let before = vm.serial.len();
        run_input(&mut vm, bad);
        assert_eq!(&vm.serial[before..], "ERR\n", "input {bad:?}");
        assert_eq!(
            vm.disk, saved_disk,
            "invalid input must not commit a record"
        );
    }
    run_input(&mut vm, "0\n65535\n");
    assert!(vm.serial.ends_with("=0\n=65535\n"));
}

#[test]
fn guest_code_buffer_has_a_fixed_term_limit_and_recovers_after_overflow() {
    let mut vm = sealed(SOURCE, config());
    let longest = format!("{}1\n", "1+".repeat(15));
    run_input(&mut vm, &longest);
    assert_eq!(vm.serial, "=16\n");
    assert_eq!(vm.disk[RECORD + 1], MAX_CODE as u8);
    assert_eq!(&vm.ram[CODE + 128..CODE + 136], &[27, 0, 0, 0, 0, 0, 0, 0]);
    let saved_disk = vm.disk.clone();
    let too_long = format!("{}1\n", "1+".repeat(16));
    run_input(&mut vm, &too_long);
    assert_eq!(vm.serial, "=16\nERR\n");
    assert_eq!(vm.disk, saved_disk);
    assert!(vm.ram[CODE - 8..CODE].iter().all(|byte| *byte == 0));
    assert!(
        vm.ram[CODE + MAX_CODE..CODE + MAX_CODE + 8]
            .iter()
            .all(|byte| *byte == 0)
    );
    run_input(&mut vm, "3*4\n");
    assert!(vm.serial.ends_with("=12\n"));
}

#[test]
fn sealed_helper_cannot_exceed_the_lifetime_cycle_budget() {
    let mut vm = sealed(
        SOURCE,
        Config {
            max_cycles: 100,
            ..config()
        },
    );
    vm.input("12+30*2\n").unwrap();
    vm.run(100_000).unwrap();
    assert_eq!(vm.status, "budget");
    assert_eq!(vm.resource_limit(), Some("cycles"));
    assert!(vm.cycles <= 100);
    assert!(vm.cycles > 0);
    let stopped = vm.export_state();
    vm.run(100_000).unwrap();
    assert_eq!(vm.export_state(), stopped);
    let consumed = vm.cycles;
    vm.reset().unwrap();
    assert_eq!(vm.cycles, consumed);
    assert!(vm.sealed);
}

#[test]
fn sealed_helper_ram_and_disk_accesses_still_fault_at_hardware_boundaries() {
    // Deliberately misallocate the bootstrap's scratch area, then seal before
    // any compiler work. The emitted ret would cross the physical RAM edge.
    let bad_allocation = SOURCE.replace(".equ CODE, 4096", ".equ CODE, RAM_BYTES - 8");
    let mut ram_vm = sealed(&bad_allocation, config());
    ram_vm.input("7\n").unwrap();
    ram_vm.run(100_000).unwrap();
    assert_eq!(ram_vm.status, "fault");
    assert_eq!(
        ram_vm.fault.as_deref().and_then(resource_limit_error),
        Some("ram")
    );
    assert_eq!(ram_vm.ram.len(), 65536);
    assert_eq!(ram_vm.serial, "");
    assert!(ram_vm.cycles > 0);

    // Enough disk for the bootstrap and record header, not the result/code.
    let mut disk_vm = sealed(
        SOURCE,
        Config {
            disk_bytes: RECORD + 2,
            ..config()
        },
    );
    disk_vm.input("7\n").unwrap();
    disk_vm.run(100_000).unwrap();
    assert_eq!(disk_vm.status, "fault");
    assert_eq!(
        disk_vm.fault.as_deref().and_then(resource_limit_error),
        Some("disk")
    );
    assert_eq!(disk_vm.disk.len(), RECORD + 2);
    assert_eq!(
        disk_vm.disk[RECORD], 0,
        "partial record must remain uncommitted"
    );
    assert!(disk_vm.cycles > 0);
}
