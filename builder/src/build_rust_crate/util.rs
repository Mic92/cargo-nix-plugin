use std::io::IsTerminal;
use std::process::Command;

pub fn echo_colored(msg: &str) {
    if std::io::stderr().is_terminal() {
        eprintln!("\x1b[0;1;32m{msg}\x1b[0m");
    } else {
        eprintln!("{msg}");
    }
}

pub fn echo_cmd(cmd: &Command) {
    let prog = cmd.get_program().to_string_lossy();
    let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy()).collect();
    if std::io::stderr().is_terminal() {
        eprint!("\x1b[0;1;32mRunning\x1b[0m");
    } else {
        eprint!("Running");
    }
    eprintln!(" {prog} {}", args.join(" "));
}

/// Run a command, print it if verbose, exit on failure.
pub fn run_cmd(cmd: &mut Command, verbose: bool) -> Result<(), Box<dyn std::error::Error>> {
    if verbose {
        echo_cmd(cmd);
    }
    let status = cmd.status()?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// Remove all .o files under a directory.
pub fn remove_object_files(dir: &str) -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new("find")
        .args([dir, "-type", "f", "-name", "*.o", "-delete"])
        .output()?;
    if !output.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}
