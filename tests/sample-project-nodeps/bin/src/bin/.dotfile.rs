// Must NOT be discovered as a bin target (cargo skips dotfiles).
compile_error!("dotfile in src/bin/ was wrongly auto-discovered");
