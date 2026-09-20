use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [command, root] if command == "mark-success" => {
            match nmbl_init::generation_state::mark_success(Path::new(root)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("nmbl-generation-state: {error}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => {
            eprintln!("usage: nmbl-generation-state mark-success IMAGE_ROOT");
            ExitCode::from(2)
        }
    }
}
