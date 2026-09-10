//! Build/ISO/QEMU automation for TarnOS.
//!
//! Run with `cargo run -p xtask -- <command>`. See [`print_usage`] for the
//! command list. This crate builds for the host and is not part of the
//! kernel/userland target set.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

const KERNEL_TARGET: &str = "x86_64-unknown-none";
const USER_TARGET_JSON: &str = "targets/x86_64-tarnos-user.json";
const LIMINE_REPO: &str = "https://github.com/limine-bootloader/limine";
const LIMINE_BRANCH: &str = "v9.x-binary";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let rest = &args[1.min(args.len())..];

    let result = match cmd {
        "build" => build(false),
        "iso" => iso(false, &[]),
        "run" => run(rest),
        "test-fault" => test_fault(),
        "test-fault-isolation" => test_fault_isolation(),
        "test-blocking-ipc" => test_blocking_ipc(),
        "test-double-send" => test_double_send(),
        "test-all" => test_fault()
            .and_then(|_| test_fault_isolation())
            .and_then(|_| test_blocking_ipc())
            .and_then(|_| test_double_send()),
        _ => {
            print_usage();
            std::process::exit(if cmd.is_empty() { 0 } else { 1 });
        }
    };

    if let Err(e) = result {
        eprintln!("xtask: error: {e}");
        std::process::exit(1);
    }
}

fn print_usage() {
    println!(
        "TarnOS build automation\n\n\
         Usage: cargo run -p xtask -- <command> [flags]\n\n\
         Commands:\n\
         \x20 build            Build the kernel and init ELF binaries\n\
         \x20 iso              Build (if needed) and assemble build/tarnos.iso\n\
         \x20 run [flags]      Build the ISO (if needed) and boot it in QEMU\n\
         \x20                    --uefi    boot via OVMF instead of legacy BIOS\n\
         \x20                    --debug   add -d int,guest_errors -D build/qemu.log -no-reboot\n\
         \x20 test-fault       Build with a deliberate page fault injected at boot and\n\
         \x20                    confirm it produces a clean panic + halt, not a triple fault\n\
         \x20 test-fault-isolation  Confirm a faulting ring-3 process is killed alone,\n\
         \x20                    not the kernel, and a second process still runs afterward\n\
         \x20 test-blocking-ipc     Confirm a process can genuinely block on sys_send with\n\
         \x20                    no receiver ready, then resume once one arrives\n\
         \x20 test-double-send      Confirm two senders with no receiver both queue\n\
         \x20                    instead of panicking (the historical bug this milestone fixed)\n\
         \x20 test-all         Run test-fault, test-fault-isolation, test-blocking-ipc,\n\
         \x20                    and test-double-send in sequence"
    );
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask always lives one directory below the workspace root")
        .to_path_buf()
}

fn run_cmd(cmd: &mut Command) -> Result<(), String> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let status: ExitStatus = cmd
        .status()
        .map_err(|e| format!("failed to spawn `{program}`: {e}"))?;
    if !status.success() {
        return Err(format!("`{program}` exited with {status}"));
    }
    Ok(())
}

/// Builds the kernel ELF against the built-in `x86_64-unknown-none` target.
///
/// That target ships with `code-model=kernel`, redzone disabled, SSE/FPU
/// disabled (softfloat), and `panic-strategy=abort` already — exactly what
/// a higher-half kernel needs — so no custom target JSON is required for
/// it. It defaults to a position-independent executable, which a fixed-
/// address higher-half kernel doesn't want, hence the explicit
/// `relocation-model=static` override.
fn build_kernel(root: &Path, release: bool, extra_features: &[&str]) -> Result<(), String> {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(root)
        .env("RUSTFLAGS", "-C relocation-model=static")
        .args([
            "build",
            "-p",
            "tarnos-kernel",
            "--target",
            KERNEL_TARGET,
            "-Zbuild-std=core,alloc,compiler_builtins",
            "-Zbuild-std-features=compiler-builtins-mem",
        ]);
    if release {
        cmd.arg("--release");
    }
    if !extra_features.is_empty() {
        cmd.args(["--features", &extra_features.join(",")]);
    }
    run_cmd(&mut cmd)
}

fn build_init(root: &Path, release: bool) -> Result<(), String> {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(root).args([
        "build",
        "-p",
        "init",
        "--target",
        USER_TARGET_JSON,
        "-Zjson-target-spec",
        "-Zbuild-std=core,alloc,compiler_builtins",
        "-Zbuild-std-features=compiler-builtins-mem",
    ]);
    if release {
        cmd.arg("--release");
    }
    run_cmd(&mut cmd)
}

fn build(release: bool) -> Result<(), String> {
    let root = workspace_root();
    build_kernel(&root, release, &[])?;
    build_init(&root, release)?;
    println!("xtask: build OK");
    Ok(())
}

fn profile_dir_name(release: bool) -> &'static str {
    if release {
        "release"
    } else {
        "debug"
    }
}

fn kernel_elf_path(root: &Path, release: bool) -> PathBuf {
    root.join("target")
        .join(KERNEL_TARGET)
        .join(profile_dir_name(release))
        .join("tarnos-kernel")
}

fn init_elf_path(root: &Path, release: bool) -> PathBuf {
    root.join("target")
        .join("x86_64-tarnos-user")
        .join(profile_dir_name(release))
        .join("init")
}

/// Ensures a working Limine checkout (with prebuilt binaries and the built
/// `limine` deploy tool) exists at `build/limine`, cloning/building it if
/// necessary. Pinned to `LIMINE_BRANCH`, which must always match the
/// protocol revision the `limine` crate dependency in kernel/Cargo.toml
/// implements.
fn ensure_limine(root: &Path) -> Result<PathBuf, String> {
    let limine_dir = root.join("build").join("limine");
    let deploy_tool = limine_dir.join("limine");

    if !limine_dir.join("limine-bios-cd.bin").exists() {
        std::fs::create_dir_all(root.join("build")).map_err(|e| e.to_string())?;
        if limine_dir.exists() {
            std::fs::remove_dir_all(&limine_dir).map_err(|e| e.to_string())?;
        }
        println!("xtask: fetching Limine ({LIMINE_BRANCH}) ...");
        run_cmd(Command::new("git").args([
            "clone",
            "--depth",
            "1",
            "--branch",
            LIMINE_BRANCH,
            LIMINE_REPO,
            limine_dir.to_str().unwrap(),
        ]))?;
    }

    if !deploy_tool.exists() {
        println!("xtask: building the limine deploy tool ...");
        run_cmd(Command::new("make").arg("limine").current_dir(&limine_dir))?;
    }

    Ok(limine_dir)
}

fn iso(release: bool, kernel_features: &[&str]) -> Result<(), String> {
    let root = workspace_root();
    build_kernel(&root, release, kernel_features)?;
    build_init(&root, release)?;
    let limine_dir = ensure_limine(&root)?;

    let iso_root = root.join("build").join("iso_root");
    let boot_dir = iso_root.join("boot");
    let efi_boot_dir = iso_root.join("EFI").join("BOOT");
    std::fs::create_dir_all(&boot_dir).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&efi_boot_dir).map_err(|e| e.to_string())?;

    let copy = |from: &Path, to: &Path| -> Result<(), String> {
        std::fs::copy(from, to)
            .map(|_| ())
            .map_err(|e| format!("copying {} -> {}: {e}", from.display(), to.display()))
    };

    copy(&kernel_elf_path(&root, release), &boot_dir.join("kernel"))?;
    copy(&init_elf_path(&root, release), &boot_dir.join("init"))?;
    copy(&root.join("limine.conf"), &boot_dir.join("limine.conf"))?;
    copy(
        &limine_dir.join("limine-bios.sys"),
        &boot_dir.join("limine-bios.sys"),
    )?;
    copy(
        &limine_dir.join("limine-bios-cd.bin"),
        &boot_dir.join("limine-bios-cd.bin"),
    )?;
    copy(
        &limine_dir.join("limine-uefi-cd.bin"),
        &boot_dir.join("limine-uefi-cd.bin"),
    )?;
    copy(
        &limine_dir.join("BOOTX64.EFI"),
        &efi_boot_dir.join("BOOTX64.EFI"),
    )?;

    let iso_path = root.join("build").join("tarnos.iso");
    println!("xtask: building {}", iso_path.display());
    run_cmd(Command::new("xorriso").current_dir(&root).args([
        "-as",
        "mkisofs",
        "-R",
        "-r",
        "-J",
        "-b",
        "boot/limine-bios-cd.bin",
        "-no-emul-boot",
        "-boot-load-size",
        "4",
        "-boot-info-table",
        "--efi-boot",
        "boot/limine-uefi-cd.bin",
        "-efi-boot-part",
        boot_dir.join("limine-uefi-cd.bin").to_str().unwrap(),
        "--protective-msdos-label",
        iso_root.to_str().unwrap(),
        "-o",
        iso_path.to_str().unwrap(),
    ]))?;

    run_cmd(
        Command::new(limine_dir.join("limine"))
            .args(["bios-install", iso_path.to_str().unwrap()]),
    )?;

    println!("xtask: iso OK -> {}", iso_path.display());
    Ok(())
}

fn find_ovmf_code() -> Option<PathBuf> {
    ["/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/ovmf/OVMF.fd"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
}

fn find_ovmf_vars_template() -> Option<PathBuf> {
    ["/usr/share/OVMF/OVMF_VARS_4M.fd"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
}

fn run(flags: &[String]) -> Result<(), String> {
    let uefi = flags.iter().any(|f| f == "--uefi");
    let debug = flags.iter().any(|f| f == "--debug");

    let root = workspace_root();
    iso(false, &[])?;

    let iso_path = root.join("build").join("tarnos.iso");
    let mut cmd = Command::new("qemu-system-x86_64");
    cmd.current_dir(&root).args([
        "-M",
        "q35",
        "-m",
        "512M",
        "-serial",
        "stdio",
        // This milestone has no framebuffer console (all output is via
        // UART), and a graphical window needs a host display server this
        // sandbox and most CI runners don't have — so default headless.
        "-display",
        "none",
        "-cdrom",
        iso_path.to_str().unwrap(),
        "-boot",
        "d",
        "-no-reboot",
        "-no-shutdown",
    ]);

    if uefi {
        let code = find_ovmf_code().ok_or("OVMF firmware not found (looked for /usr/share/OVMF/OVMF_CODE_4M.fd)")?;
        let vars_template = find_ovmf_vars_template()
            .ok_or("OVMF vars template not found (looked for /usr/share/OVMF/OVMF_VARS_4M.fd)")?;
        let vars_copy = root.join("build").join("OVMF_VARS.fd");
        std::fs::copy(&vars_template, &vars_copy).map_err(|e| e.to_string())?;
        cmd.args([
            "-drive",
            &format!("if=pflash,format=raw,readonly=on,file={}", code.display()),
            "-drive",
            &format!("if=pflash,format=raw,file={}", vars_copy.display()),
        ]);
    }

    if debug {
        let log_path = root.join("build").join("qemu.log");
        cmd.args([
            "-d",
            "int,guest_errors",
            "-D",
            log_path.to_str().unwrap(),
        ]);
    }

    println!("xtask: launching QEMU ({})", if uefi { "UEFI" } else { "BIOS" });
    run_cmd(&mut cmd)
}

/// Builds the kernel with `kernel_features` enabled, boots it in QEMU
/// with serial output redirected to `<build>/<log_name>`, waits a fixed
/// window (these test scenarios have no way to signal "done" on their
/// own — most end in a deliberate halt loop — so this kills QEMU after
/// giving it time to reach that point rather than waiting for it to
/// exit), and returns the captured log. Shared by every milestone-2
/// integration test scenario below; each one builds with its own
/// feature and applies its own assertions to the returned log.
fn run_scenario(kernel_features: &[&str], log_name: &str, timeout_secs: u64) -> Result<String, String> {
    let root = workspace_root();
    iso(false, kernel_features)?;

    let iso_path = root.join("build").join("tarnos.iso");
    let log_path = root.join("build").join(log_name);
    let _ = std::fs::remove_file(&log_path);

    let mut child = Command::new("qemu-system-x86_64")
        .current_dir(&root)
        .args([
            "-M",
            "q35",
            "-m",
            "512M",
            "-serial",
            &format!("file:{}", log_path.display()),
            "-display",
            "none",
            "-cdrom",
            iso_path.to_str().unwrap(),
            "-boot",
            "d",
            "-no-reboot",
            "-no-shutdown",
        ])
        .spawn()
        .map_err(|e| format!("failed to spawn qemu-system-x86_64: {e}"))?;

    std::thread::sleep(std::time::Duration::from_secs(timeout_secs));
    let _ = child.kill();
    let _ = child.wait();

    let log = std::fs::read_to_string(&log_path)
        .map_err(|e| format!("reading {}: {e}", log_path.display()))?;
    println!("xtask: captured serial output:\n{log}");
    Ok(log)
}

/// Confirms the guest booted exactly once — the guest resetting instead
/// of cleanly halting (a triple fault, or a panic loop under
/// `-no-reboot` somehow not actually halting) would show up as a second
/// "TarnOS booting" line.
fn assert_booted_once(log: &str) -> Result<(), String> {
    let boot_lines = log.matches("TarnOS booting").count();
    if boot_lines != 1 {
        return Err(format!(
            "expected exactly one boot (\"TarnOS booting\" once); saw {boot_lines} — \
             looks like the guest reset instead of halting"
        ));
    }
    Ok(())
}

/// Builds the kernel with a deliberate page-fault-at-boot injected (the
/// `fault-injection-test` feature — see `kernel/src/main.rs`), boots it,
/// and checks the serial output for a clean panic + halt rather than a
/// triple fault (which under QEMU with `-no-reboot` would otherwise show
/// up as the guest resetting instead of printing a diagnostic). This is
/// the permanent, repeatable form of the same check done by hand back
/// when the double-fault IST stack was first wired up. Reaches this
/// fault while still in ring 0 (early boot code), so it must still
/// panic the whole kernel — see `test_fault_isolation` for the
/// ring-3 (process-only) case.
fn test_fault() -> Result<(), String> {
    let log = run_scenario(&["fault-injection-test"], "fault-test.log", 3)?;
    assert_booted_once(&log)?;
    if !log.contains("[KERNEL PANIC]") || !log.contains("page fault") {
        return Err(
            "expected a \"[KERNEL PANIC] ... page fault ...\" line in the serial output, \
             but didn't find one"
                .to_string(),
        );
    }
    println!("xtask: test-fault PASSED — one clean panic + halt, no triple fault / reboot loop");
    Ok(())
}

/// Milestone 2 workstream B: builds the kernel with two dummy ring-3
/// processes (the `fault-isolation-test` feature) — one that
/// dereferences a bad pointer, one that exits cleanly — and confirms
/// the fault kills only the offending process: no kernel panic, and the
/// survivor still reaches the scheduler's normal "last process exited"
/// halt afterward. This is what distinguishes real process isolation
/// from merely not crashing: the machine keeps doing useful work after
/// a process misbehaves.
fn test_fault_isolation() -> Result<(), String> {
    let log = run_scenario(&["fault-isolation-test"], "fault-isolation-test.log", 3)?;
    assert_booted_once(&log)?;
    if log.contains("[KERNEL PANIC]") {
        return Err(
            "expected no kernel panic -- a ring-3 fault should kill only the offending \
             process, not the kernel"
                .to_string(),
        );
    }
    if !log.contains("[fault]") || !log.contains("killed") {
        return Err(
            "expected a \"[fault] pid ... killed: ...\" line showing the faulting process \
             was terminated"
                .to_string(),
        );
    }
    if !log.contains("last process exited, halting") {
        return Err(
            "expected the survivor process to still reach a clean exit after the other \
             process faulted"
                .to_string(),
        );
    }
    println!(
        "xtask: test-fault-isolation PASSED — faulting process killed, survivor still ran \
         to completion, no kernel panic"
    );
    Ok(())
}

/// Milestone 2 workstream C: builds the kernel with the console server
/// deliberately left unpolled before init runs (the `blocking-ipc-test`
/// feature), forcing init's first `sys_send` to find nobody receiving
/// and genuinely block, rather than the normal boot's always-primed-
/// receiver ordering. Confirms init's message still arrives once the
/// scheduler gives the console server its first chance to run — proving
/// a process can actually suspend and later resume, not merely that the
/// demo's usual ordering happens to avoid ever needing to.
fn test_blocking_ipc() -> Result<(), String> {
    let log = run_scenario(&["blocking-ipc-test"], "blocking-ipc-test.log", 5)?;
    assert_booted_once(&log)?;
    if log.contains("[KERNEL PANIC]") {
        return Err("expected no kernel panic".to_string());
    }
    if !log.contains("Hello from TarnOS userspace!") {
        return Err(
            "expected init's greeting to still arrive after genuinely blocking on sys_send"
                .to_string(),
        );
    }
    println!(
        "xtask: test-blocking-ipc PASSED — init's send blocked with no receiver ready and \
         still delivered once the console server was polled"
    );
    Ok(())
}

/// Milestone 2 workstream C (the specific bug it fixes): builds the
/// kernel with two dummy ring-3 processes that both send on the same
/// endpoint before any receiver is ever polled (the `double-send-test`
/// feature) — the exact historical scenario where a second sender with
/// nobody receiving panicked the kernel. Confirms both sends queue and
/// are eventually delivered instead.
fn test_double_send() -> Result<(), String> {
    let log = run_scenario(&["double-send-test"], "double-send-test.log", 5)?;
    assert_booted_once(&log)?;
    if log.contains("[KERNEL PANIC]") {
        return Err(
            "expected no kernel panic -- a second sender with nobody receiving must queue, \
             not panic"
                .to_string(),
        );
    }
    if !log.contains("MSGA") || !log.contains("MSGB") {
        return Err(
            "expected both queued senders' messages (\"MSGA\" and \"MSGB\") to have been \
             delivered"
                .to_string(),
        );
    }
    if !log.contains("last process exited, halting") {
        return Err("expected both sender processes to reach a clean exit".to_string());
    }
    println!(
        "xtask: test-double-send PASSED — two senders with no receiver both queued and were \
         delivered, no kernel panic"
    );
    Ok(())
}
