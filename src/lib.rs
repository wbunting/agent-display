//! Rust port of `agent-display` (wbunting/dotfiles skills/agent-display):
//! a private headless sway display, OS input and agent-only Chromium for
//! agents on the Omarchy host, plus `tab` commands to drive a tab over CDP.

pub mod browser;
pub mod cdp;
pub mod cli;
pub mod cli_text;
pub mod ctx;
pub mod display;
pub mod hold_input;
pub mod json;
pub mod net;
pub mod sway;
pub mod systemd;
pub mod tab;
pub mod typing;
pub mod util;

#[cfg(test)]
mod tests;

use cli::Cmd;
use ctx::Ctx;

/// Run one command line (without argv[0]); returns the exit status.
pub fn main_with(ctx: &Ctx, argv: &[String]) -> i32 {
    let cmd = match cli::parse(argv) {
        Ok(c) => c,
        Err(exit) => {
            let _ = ctx.out.borrow_mut().write_all(exit.stdout.as_bytes());
            let _ = ctx.err.borrow_mut().write_all(exit.stderr.as_bytes());
            return exit.code;
        }
    };
    let is_tab = matches!(
        cmd,
        Cmd::TabOpen { .. } | Cmd::TabGoto { .. } | Cmd::TabEval { .. } | Cmd::TabShot { .. }
            | Cmd::TabInfo { .. } | Cmd::TabClose { .. } | Cmd::TabList { .. }
    );
    match dispatch(ctx, &cmd) {
        Ok(code) => code,
        Err(e) => {
            // tab commands report socket errors as `CDP: ...` (the Python's
            // OSError handler); others print the message as is.
            let msg = if is_tab { tab::cdp_text(&e) } else { e.msg };
            ctx.eprintln(&format!("agent-display: {msg}"));
            1
        }
    }
}

use std::io::Write;

fn dispatch(ctx: &Ctx, cmd: &Cmd) -> util::Result<i32> {
    use Cmd::*;
    match cmd {
        Check => display::cmd_check(ctx),
        Start { size, renderer } => display::cmd_start(ctx, size, renderer.as_deref()),
        Stop => display::cmd_stop(ctx),
        Status => display::cmd_status(ctx),
        Env => display::cmd_env(ctx),
        Exec { argv } => display::cmd_exec(ctx, argv),
        Launch { name, argv } => display::cmd_launch(ctx, name, argv),
        Windows => display::cmd_windows(ctx),
        Focus { id } => display::cmd_focus(ctx, *id),
        Screenshot { path, window } => display::cmd_screenshot(ctx, path, *window),
        Type { text } => display::cmd_type(ctx, text),
        Key { key, mods } => display::cmd_key(ctx, key, mods),
        Move { x, y } => display::cmd_move(ctx, *x, *y),
        Click { button, at, count } => display::cmd_click(ctx, button, *at, *count),
        Scroll { direction, steps } => display::cmd_scroll(ctx, direction, *steps),
        BrowserStart { temp, port, audio, urls } => browser::cmd_browser_start(ctx, *temp, *port, *audio, urls),
        BrowserStop { unit, all } => browser::cmd_browser_stop(ctx, unit.as_deref(), *all),
        BrowserList => browser::cmd_browser_list(ctx),
        TabOpen { o, url } => tab::cmd_tab_open(ctx, o, url.as_deref()),
        TabGoto { o, tab, url, wait_ms } => tab::cmd_tab_goto(ctx, o, tab, url, *wait_ms),
        TabEval { o, tab, js } => tab::cmd_tab_eval(ctx, o, tab, js),
        TabShot { o, tab, path, full_page } => tab::cmd_tab_shot(ctx, o, tab, path, *full_page),
        TabInfo { o, tab } => tab::cmd_tab_info(ctx, o, tab),
        TabClose { o, tab } => tab::cmd_tab_close(ctx, o, tab),
        TabList { o } => tab::cmd_tab_list(ctx, o),
        View { port, view_only } => display::cmd_view(ctx, *port, *view_only),
        Unview => display::cmd_unview(ctx),
        HoldInput => display::cmd_hold_input(ctx),
    }
}
