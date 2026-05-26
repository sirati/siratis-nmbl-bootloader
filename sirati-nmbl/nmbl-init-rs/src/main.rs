// nmbl-init — PID 1 inside the NMBL initramfs.
//
// This is currently a placeholder so the crate compiles. The real entry point
// is described in PLAN.md and will land module-by-module per the roadmap.

fn main() -> std::process::ExitCode {
    eprintln!("nmbl-init: skeleton build — implementation pending (see PLAN.md)");
    std::process::ExitCode::from(0)
}
