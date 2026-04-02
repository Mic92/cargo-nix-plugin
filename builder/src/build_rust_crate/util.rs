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

/// Run a command, printing it if verbose. Exits on failure.
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

/// Remove .o files under a directory tree to avoid "wrong ELF type" errors.
pub fn remove_object_files(dir: &str) -> Result<(), Box<dyn std::error::Error>> {
    fn walk(dir: &std::path::Path) -> std::io::Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                walk(&path)?;
            } else if path.extension().map(|e| e == "o").unwrap_or(false) {
                std::fs::remove_file(&path)?;
            }
        }
        Ok(())
    }
    walk(std::path::Path::new(dir))?;
    Ok(())
}
