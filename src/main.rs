use agent_display::ctx::{Ctx, RealSys};
use agent_display::util::Environ;

fn main() {
    let argv: Vec<String> = std::env::args_os().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
    let sys = RealSys;
    let ctx = Ctx::new(Environ::from_process(), &sys, Box::new(std::io::stdout()), Box::new(std::io::stderr()));
    let code = agent_display::main_with(&ctx, &argv);
    drop(ctx);
    std::process::exit(code & 0xff);
}
