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
        "test-uefi-boot" => test_uefi_boot(),
        "test-spawn-ipc" => test_spawn_ipc(),
        "test-spawn-boundary" => test_spawn_boundary(),
        "test-process-lifecycle" => test_process_lifecycle(),
        "test-wait-exit-code" => test_wait_exit_code(),
        "test-kill-boundary" => test_kill_boundary(),
        "test-all" => test_fault()
            .and_then(|_| test_fault_isolation())
            .and_then(|_| test_blocking_ipc())
            .and_then(|_| test_double_send())
            .and_then(|_| test_uefi_boot())
            .and_then(|_| test_spawn_ipc())
            .and_then(|_| test_spawn_boundary())
            .and_then(|_| test_process_lifecycle())
            .and_then(|_| test_wait_exit_code())
            .and_then(|_| test_kill_boundary()),
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
         \x20 test-uefi-boot        Confirm the normal boot sequence also completes\n\
         \x20                    end to end via UEFI/OVMF, not just BIOS\n\
         \x20 test-spawn-ipc        Confirm init can dynamically spawn a second process,\n\
         \x20                    grant it a capability, release it, and complete a real IPC\n\
         \x20                    round trip with it -- not boot-choreographed\n\
         \x20 test-spawn-boundary   Confirm SYS_GRANT/SYS_PROCESS_START reject a non-child\n\
         \x20                    target and a rights-amplifying grant, while a legitimate\n\
         \x20                    grant+start still succeeds\n\
         \x20 test-process-lifecycle  Confirm repeated spawn+kill cycles well beyond\n\
         \x20                    MAX_PROCESSES never exhaust the process table or leak memory\n\
         \x20 test-wait-exit-code    Confirm SYS_WAIT genuinely blocks on a not-yet-run\n\
         \x20                    child and reports the exit code it actually passed to sys_exit\n\
         \x20 test-kill-boundary     Confirm SYS_KILL rejects a non-child target, while\n\
         \x20                    legitimate kills against a Suspended and a Blocked child succeed\n\
         \x20 test-all         Run test-fault, test-fault-isolation, test-blocking-ipc,\n\
         \x20                    test-double-send, test-uefi-boot, test-spawn-ipc,\n\
         \x20                    test-spawn-boundary, test-process-lifecycle,\n\
         \x20                    test-wait-exit-code, and test-kill-boundary in sequence"
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

/// Builds one userland crate against the custom `x86_64-tarnos-user`
/// target. `package` is the Cargo package name (e.g. `"init"`,
/// `"echo-child"`) — every userland binary built this way ends up at
/// the same `target/x86_64-tarnos-user/<profile>/<package>` path
/// `user_elf_path` computes, since Cargo names the output after the
/// `[[bin]]` target, which every userland `Cargo.toml` here sets equal
/// to its package name.
fn build_user_crate(root: &Path, release: bool, package: &str) -> Result<(), String> {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(root).args([
        "build",
        "-p",
        package,
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

/// Every userland binary the ISO ships — `init` (boot-loaded directly)
/// plus every program `SYS_SPAWN` can create a process from by name
/// (see `task::process::init_spawnable_modules`). Building and shipping
/// all of them unconditionally, for every scenario, keeps `limine.conf`
/// (which declares a fixed set of boot modules) valid regardless of
/// which kernel feature a given `xtask` command builds with — none of
/// the existing milestone-2 test scenarios exercise spawning, but they
/// still boot the same `limine.conf`.
const USER_CRATES: &[&str] = &["init", "echo-child", "exit-code-child"];

fn build(release: bool) -> Result<(), String> {
    let root = workspace_root();
    build_kernel(&root, release, &[])?;
    for package in USER_CRATES {
        build_user_crate(&root, release, package)?;
    }
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

fn user_elf_path(root: &Path, release: bool, package: &str) -> PathBuf {
    root.join("target")
        .join("x86_64-tarnos-user")
        .join(profile_dir_name(release))
        .join(package)
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
    for package in USER_CRATES {
        build_user_crate(&root, release, package)?;
    }
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
    for package in USER_CRATES {
        copy(
            &user_elf_path(&root, release, package),
            &boot_dir.join(package),
        )?;
    }
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
    [
        "/usr/share/OVMF/OVMF_CODE_4M.fd",
        "/usr/share/OVMF/OVMF_CODE.fd",
        "/usr/share/ovmf/OVMF.fd",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
}

fn find_ovmf_vars_template() -> Option<PathBuf> {
    [
        "/usr/share/OVMF/OVMF_VARS_4M.fd",
        "/usr/share/OVMF/OVMF_VARS.fd",
    ]
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
/// (BIOS, unless `uefi` is set) with serial output redirected to
/// `<build>/<log_name>`, waits a fixed window (these test scenarios
/// have no way to signal "done" on their own — most end in a deliberate
/// halt loop — so this kills QEMU after giving it time to reach that
/// point rather than waiting for it to exit), and returns the captured
/// log. Shared by every milestone-2 integration test scenario below;
/// each one builds with its own feature and applies its own assertions
/// to the returned log.
fn run_scenario(
    kernel_features: &[&str],
    log_name: &str,
    timeout_secs: u64,
    uefi: bool,
) -> Result<String, String> {
    let root = workspace_root();
    iso(false, kernel_features)?;

    let iso_path = root.join("build").join("tarnos.iso");
    let log_path = root.join("build").join(log_name);
    let _ = std::fs::remove_file(&log_path);

    let mut cmd = Command::new("qemu-system-x86_64");
    cmd.current_dir(&root).args([
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
    ]);

    if uefi {
        let code = find_ovmf_code()
            .ok_or("OVMF firmware not found (looked for /usr/share/OVMF/OVMF_CODE_4M.fd)")?;
        let vars_template = find_ovmf_vars_template()
            .ok_or("OVMF vars template not found (looked for /usr/share/OVMF/OVMF_VARS_4M.fd)")?;
        // A name distinct from `run`'s own `OVMF_VARS.fd` copy, so a
        // `test-uefi-boot` run doesn't race a concurrent `run --uefi`
        // over the same file.
        let vars_copy = root.join("build").join("OVMF_VARS_test.fd");
        std::fs::copy(&vars_template, &vars_copy).map_err(|e| e.to_string())?;
        cmd.args([
            "-drive",
            &format!("if=pflash,format=raw,readonly=on,file={}", code.display()),
            "-drive",
            &format!("if=pflash,format=raw,file={}", vars_copy.display()),
        ]);
    }

    let mut child = cmd
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

/// Extracts every `free_frames=<N>` value the kernel logged, in the
/// order they appear — `task::scheduler::switch_to_next_or_halt` prints
/// one at every halt, and `process-lifecycle-test`'s boot block prints
/// a matching one right before the scheduler starts, giving
/// `test_process_lifecycle` a before/after pair to compare.
fn extract_free_frame_counts(log: &str) -> Vec<u64> {
    log.lines()
        .filter_map(|line| line.split("free_frames=").nth(1))
        .filter_map(|rest| rest.trim().parse::<u64>().ok())
        .collect()
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

/// Boots the normal (no test feature) kernel via UEFI/OVMF and confirms
/// it reaches the same end-to-end result as a BIOS boot: init's greeting
/// arriving over the full syscall/capability/IPC path. `xtask run --uefi`
/// already exercises this manually, but with no automated pass/fail
/// signal and no fixed timeout (it would hang a CI job forever once the
/// kernel reaches its terminal halt) — this is the permanent, CI-safe
/// form of that same check, added after this milestone's earlier
/// (BIOS-only) test-fault regression test shipped without a UEFI
/// counterpart, even though an intermittent UEFI-only boot hang was one
/// of the very issues this milestone's hardening work fixed.
fn test_uefi_boot() -> Result<(), String> {
    let log = run_scenario(&[], "uefi-boot-test.log", 8, true)?;
    assert_booted_once(&log)?;
    if log.contains("[KERNEL PANIC]") {
        return Err("expected no kernel panic on a normal UEFI boot".to_string());
    }
    if !log.contains("Hello from TarnOS userspace!") {
        return Err(
            "expected init's greeting to arrive over UEFI, same as it does over BIOS"
                .to_string(),
        );
    }
    println!("xtask: test-uefi-boot PASSED — normal boot completed end to end via UEFI/OVMF");
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
    let log = run_scenario(&["fault-injection-test"], "fault-test.log", 5, false)?;
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
    let log = run_scenario(&["fault-isolation-test"], "fault-isolation-test.log", 5, false)?;
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
    let log = run_scenario(&["blocking-ipc-test"], "blocking-ipc-test.log", 8, false)?;
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
    let log = run_scenario(&["double-send-test"], "double-send-test.log", 8, false)?;
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

/// Milestone 3: boots the *normal, unconditional* boot sequence (no test
/// feature — `init` always does this now) and confirms the whole
/// dynamic-process-creation chain works end to end: `init` spawns
/// `echo-child` (a process boot code never mentions at all), grants it a
/// capability it starts with none of, releases it with
/// `SYS_PROCESS_START`, and completes a genuine rendezvous with it —
/// none of it boot-choreographed the way the console-server handoff is.
fn test_spawn_ipc() -> Result<(), String> {
    let log = run_scenario(&[], "spawn-ipc-test.log", 8, false)?;
    assert_booted_once(&log)?;
    if log.contains("[KERNEL PANIC]") {
        return Err("expected no kernel panic".to_string());
    }
    if !log.contains("Hello from TarnOS userspace!") {
        return Err("expected init's own greeting (regression check)".to_string());
    }
    if !log.contains("child replied: pong") {
        return Err(
            "expected \"child replied: pong\" -- init's dynamically spawned echo-child should \
             have replied over the granted capability"
                .to_string(),
        );
    }
    if !log.contains("last process exited, halting") {
        return Err("expected both init and its spawned child to reach a clean exit".to_string());
    }
    println!(
        "xtask: test-spawn-ipc PASSED — init dynamically spawned echo-child, granted it a \
         capability, and completed a real IPC round trip with it"
    );
    Ok(())
}

/// Milestone 3, adversarially: builds the kernel with two dummy ring-3
/// processes (the `spawn-boundary-test` feature) — an idle bystander,
/// and a test process that probes `SYS_GRANT`/`SYS_PROCESS_START`'s
/// ownership and rights checks directly (a grant against a real
/// process that isn't its child, a grant requesting rights it doesn't
/// hold, then a legitimate spawn+grant+start) — and confirms all four
/// checks matched their expected result. Complements `test_spawn_ipc`:
/// that one proves the happy path works, this one proves the boundary
/// is actually enforced, not merely unexercised.
fn test_spawn_boundary() -> Result<(), String> {
    let log = run_scenario(&["spawn-boundary-test"], "spawn-boundary-test.log", 5, false)?;
    assert_booted_once(&log)?;
    if log.contains("[KERNEL PANIC]") {
        return Err("expected no kernel panic".to_string());
    }
    if log.contains("BOUNDARY_FAIL") {
        return Err(
            "boundary-test process reported BOUNDARY_FAIL -- SYS_GRANT/SYS_PROCESS_START did \
             not enforce ownership/rights the way it should have"
                .to_string(),
        );
    }
    if !log.contains("BOUNDARY_OK") {
        return Err(
            "expected \"BOUNDARY_OK\" -- the boundary-test process never reported a result at \
             all"
                .to_string(),
        );
    }
    println!(
        "xtask: test-spawn-boundary PASSED — a grant against a non-child and a rights-\
         amplifying grant were both rejected, while a legitimate grant+start still succeeded"
    );
    Ok(())
}

/// Milestone 4: builds the kernel with a single dummy ring-3 process
/// (the `process-lifecycle-test` feature) that repeatedly spawns a
/// `Suspended` `echo-child` and immediately kills it, 3 * `MAX_PROCESSES`
/// times in a row — far more than the process table's 16 slots could
/// ever survive if terminating a process didn't actually free its slot
/// and physical memory. Confirms the loop completes (no
/// `ResourceExhausted`/`SpawnFailed` partway through) and that the
/// physical frame allocator reports the exact same free-frame count
/// before the loop starts and after the run halts.
fn test_process_lifecycle() -> Result<(), String> {
    let log = run_scenario(
        &["process-lifecycle-test"],
        "process-lifecycle-test.log",
        8,
        false,
    )?;
    assert_booted_once(&log)?;
    if log.contains("[KERNEL PANIC]") {
        return Err("expected no kernel panic".to_string());
    }
    if log.contains("LIFECYCLE_FAIL") {
        return Err(
            "lifecycle-test process reported LIFECYCLE_FAIL -- a spawn or kill failed \
             partway through the loop, suggesting the process table is leaking"
                .to_string(),
        );
    }
    if !log.contains("LIFECYCLE_OK") {
        return Err(
            "expected \"LIFECYCLE_OK\" -- the lifecycle-test process never reported a result \
             at all"
                .to_string(),
        );
    }
    match extract_free_frame_counts(&log).as_slice() {
        [before, after] if before == after => {}
        [before, after] => {
            return Err(format!(
                "expected the free physical frame count to return to its starting value \
                 after 48 spawn+kill cycles, but it went from {before} to {after} -- \
                 AddressSpace teardown is leaking physical memory"
            ));
        }
        other => {
            return Err(format!(
                "expected exactly two \"free_frames=\" log lines (before and after), found {}",
                other.len()
            ));
        }
    }
    println!(
        "xtask: test-process-lifecycle PASSED — 48 spawn+kill cycles completed without \
         exhausting the process table, and physical memory usage returned to baseline"
    );
    Ok(())
}

/// Milestone 4: builds the kernel with a single dummy ring-3 process
/// (the `wait-exit-code-test` feature) that spawns `exit-code-child`,
/// releases it, and immediately `SYS_WAIT`s on it before it has ever
/// run — deterministically forcing the wait to genuinely block and
/// later resume, rather than merely reading an already-`Zombie` slot.
/// Confirms the reported status matches the exact code
/// `exit-code-child` passes to `sys_exit`.
fn test_wait_exit_code() -> Result<(), String> {
    let log = run_scenario(&["wait-exit-code-test"], "wait-exit-code-test.log", 8, false)?;
    assert_booted_once(&log)?;
    if log.contains("[KERNEL PANIC]") {
        return Err("expected no kernel panic".to_string());
    }
    if log.contains("WAIT_FAIL") {
        return Err(
            "wait-test process reported WAIT_FAIL -- SYS_WAIT did not report the exit code \
             exit-code-child actually passed to sys_exit"
                .to_string(),
        );
    }
    if !log.contains("WAIT_OK") {
        return Err(
            "expected \"WAIT_OK\" -- the wait-test process never reported a result at all"
                .to_string(),
        );
    }
    println!(
        "xtask: test-wait-exit-code PASSED — SYS_WAIT genuinely blocked on a not-yet-run \
         child and reported its correct exit code once it exited"
    );
    Ok(())
}

/// Milestone 4, adversarially: builds the kernel with two dummy ring-3
/// processes (the `kill-boundary-test` feature) — an idle bystander,
/// and a test process that probes `SYS_KILL`'s ownership check directly
/// (a kill against a real process that isn't its child), then proves a
/// legitimate kill still works against both a `Suspended` child and a
/// genuinely `Blocked` one. Complements `test_spawn_boundary`'s bar for
/// adversarial proof, applied to termination instead of grant/start.
fn test_kill_boundary() -> Result<(), String> {
    let log = run_scenario(&["kill-boundary-test"], "kill-boundary-test.log", 5, false)?;
    assert_booted_once(&log)?;
    if log.contains("[KERNEL PANIC]") {
        return Err("expected no kernel panic".to_string());
    }
    if log.contains("KILL_FAIL") {
        return Err(
            "kill-test process reported KILL_FAIL -- SYS_KILL did not enforce ownership, or \
             a legitimate kill against a Suspended/Blocked child did not succeed"
                .to_string(),
        );
    }
    if !log.contains("KILL_OK") {
        return Err(
            "expected \"KILL_OK\" -- the kill-test process never reported a result at all"
                .to_string(),
        );
    }
    println!(
        "xtask: test-kill-boundary PASSED — a kill against a non-child was rejected, while \
         legitimate kills against both a Suspended and a genuinely Blocked child succeeded"
    );
    Ok(())
}
