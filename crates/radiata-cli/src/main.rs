fn main() {
    // Allocator tuning before any thread spawns: the session-load and
    // attach-snapshot phases are large transient bursts, and glibc's
    // per-thread arenas otherwise keep each burst's high-water pages
    // resident for the process lifetime.
    radiata_types::memory_release::cap_thread_arenas();
    let args: Vec<String> = std::env::args().skip(1).collect();
    // A fresh top-level invocation sheds an inherited worker identity before
    // any thread exists (edition 2021 makes `remove_var` safe, but it must
    // still run single-threaded): everything but the supervisor-spawned
    // `worker` subcommand.
    if args.first().map(String::as_str) != Some("worker") {
        radiata_daemon::worker::strip_inherited_worker_env();
    }
    let code = radiata_cli::main_with_runtime(&args, &radiata_cli::PrintRuntime);
    // Restore the Windows VT bit before process exit.
    radiata_types::platform::console_restore();
    std::process::exit(code);
}
