//! macOS: `foil vm`, the Linux-VM mode for code you don't trust at all.
//!
//! Seatbelt shares the Mac's kernel with the code it contains, and leaves
//! some things reachable (system services, the process list). For hostile
//! code the stronger answer is a separate kernel: Linux foil, with every
//! Linux guarantee, inside a small Linux virtual machine. Lima runs that VM
//! with Apple's Virtualization framework; only the folders named at setup
//! are shared into it, so even a kernel exploit inside reaches nothing else.
//!
//!   foil vm setup [DIR...]   create the VM (shares DIR..., default: .) and install foil in it
//!   foil vm [foil args]      run foil inside the VM, in the same folder
//!   foil vm stop | delete

use std::ffi::OsString;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

/// The Lima instance foil uses.
const INSTANCE: &str = "foil";

/// API keys passed into the VM so foil's proxy there can inject them.
const KEYS: [&str; 2] = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"];

pub fn main(args: &[OsString]) -> io::Result<()> {
    if Command::new("limactl").arg("--version").output().is_err() {
        return Err(io::Error::other("foil vm needs Lima: brew install lima"));
    }
    match args.first().and_then(|a| a.to_str()) {
        Some("setup") => setup(&args[1..]),
        Some("stop") => run(Command::new("limactl").args(["stop", INSTANCE])),
        Some("delete") => run(Command::new("limactl").args(["delete", "--force", INSTANCE])),
        _ => {
            // foil vm <foil args>: the same command, inside the VM, in the same folder.
            let cwd = std::env::current_dir()?;
            // Only folders shared at setup exist inside the VM.
            let shared = Command::new("limactl").args(["shell", INSTANCE, "test", "-d"]).arg(&cwd).output()?;
            if !shared.status.success() {
                return Err(io::Error::other(format!(
                    "{} isn't shared with foil's VM (or the VM isn't running); `foil vm delete`, \
                     then `foil vm setup` with the folders it should see",
                    cwd.display()
                )));
            }
            let mut cmd = Command::new("limactl");
            cmd.args(["shell", "--workdir"]).arg(&cwd).arg(INSTANCE).arg("env");
            for key in KEYS {
                if let Some(value) = std::env::var_os(key) {
                    let mut kv = OsString::from(format!("{key}="));
                    kv.push(value);
                    cmd.arg(kv);
                }
            }
            cmd.arg("foil").args(args);
            Err(cmd.exec())
        }
    }
}

/// Create and start the VM, sharing only `dirs` (writable), then install
/// foil in it from the latest release.
fn setup(dirs: &[OsString]) -> io::Result<()> {
    let dirs: Vec<PathBuf> = if dirs.is_empty() {
        vec![std::env::current_dir()?]
    } else {
        dirs.iter().map(|d| PathBuf::from(d).canonicalize()).collect::<Result<_, _>>()?
    };
    let mounts: Vec<String> = dirs.iter().map(|d| format!("{{\"location\":{:?},\"writable\":true}}", d.display().to_string())).collect();
    let exists = Command::new("limactl").args(["list", "--quiet"]).output()?.stdout.split(|&b| b == b'\n').any(|l| l == INSTANCE.as_bytes());
    if exists {
        eprintln!("foil vm: the VM exists already (`foil vm delete` to start over); starting it");
    } else {
        eprintln!("foil vm: creating the VM (Ubuntu, sharing {})", dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", "));
        run(Command::new("limactl")
            .args(["create", "--tty=false", "--name", INSTANCE, "--set"])
            .arg(format!(".mounts = [{}]", mounts.join(",")))
            .arg("template://ubuntu-lts"))?;
    }
    run(Command::new("limactl").args(["start", "--tty=false", INSTANCE]))?;
    eprintln!("foil vm: installing foil inside");
    run(Command::new("limactl").args(["shell", INSTANCE, "sh", "-c"]).arg(
        "curl -fsSL https://raw.githubusercontent.com/umbarsariya/foil/main/install.sh | sh",
    ))?;
    eprintln!("foil vm: ready; try `foil vm doctor`, then `foil vm run -b NAME --strict -- ...` in a shared folder");
    Ok(())
}

fn run(cmd: &mut Command) -> io::Result<()> {
    let status = cmd.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{cmd:?} failed ({status})")))
    }
}
