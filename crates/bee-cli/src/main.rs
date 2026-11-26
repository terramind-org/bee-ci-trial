//! bee: the command-line client, `--json` on every command.

fn main() {
    let code = bee_cli::run(std::env::args_os());
    std::process::exit(code);
}
