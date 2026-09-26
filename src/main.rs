use std::ffi::OsString;
use std::io::ErrorKind;
use std::process::exit;

use foil::supervisor::{exit_like, supervise};

const USAGE: &str = "usage: foil run [--] <command> [args...]";

fn main() {
    let mut args = std::env::args_os().skip(1);
    if args.next().map_or(true, |a| a != "run") {
        eprintln!("{USAGE}");
        exit(2);
    }
    let mut command: Vec<OsString> = args.collect();
    if command.first().is_some_and(|a| a == "--") {
        command.remove(0);
    }
    if command.is_empty() {
        eprintln!("{USAGE}");
        exit(2);
    }

    match supervise(&command) {
        Ok(outcome) => exit_like(outcome.termination),
        Err(err) => {
            eprintln!("foil: {}: {err}", command[0].to_string_lossy());
            // Same conventions as POSIX shells and `docker run`.
            exit(match err.kind() {
                ErrorKind::NotFound => 127,
                ErrorKind::PermissionDenied => 126,
                _ => 125,
            })
        }
    }
}
