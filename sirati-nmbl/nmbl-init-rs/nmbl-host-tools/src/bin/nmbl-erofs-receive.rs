//! `nmbl-erofs-receive INCOMING_ROOT IMAGE_ROOT PUBLIC_KEY`: install and
//! activate the signed generation bundle on stdin. See
//! [`nmbl_host_tools::receive`].

use std::process::ExitCode;

fn main() -> ExitCode {
    match nmbl_host_tools::receive::main_with(nmbl_host_tools::receive::args()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nmbl-erofs-receive: {error}");
            ExitCode::FAILURE
        }
    }
}
