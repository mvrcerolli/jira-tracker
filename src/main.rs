mod app;
mod cache;
mod config;
mod filter;
mod github;
mod jira;
mod ui;
mod view;

use crossterm::event::{self, Event, KeyEventKind};
use std::{sync::mpsc, time::Duration};

fn main() -> anyhow::Result<()> {
    let cfg = match config::load() {
        Ok(c) => c,
        Err(e) => fail(&e.to_string()),
    };
    let client = match jira::Client::new(cfg) {
        Ok(c) => c,
        Err(e) => fail(&e.to_string()),
    };
    let (tx, rx) = mpsc::channel();
    let mut app = app::App::new(client, tx);

    let mut term = ratatui::init();
    let res = (|| -> anyhow::Result<()> {
        while !app.quit {
            while let Ok(m) = rx.try_recv() {
                app.on_msg(m);
            }
            app.tick();
            term.draw(|f| ui::draw(f, &mut app))?;
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            {
                app.on_key(k);
            }
        }
        Ok(())
    })();
    ratatui::restore();
    app.save_cache();
    if let Err(e) = res {
        fail(&format!("Error: {e}"));
    }
    Ok(())
}

/// Print an error and exit. On Windows, wait for Enter so a double-clicked
/// console window doesn't close before the message can be read.
fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    if cfg!(windows) {
        eprintln!("\nPress Enter to close…");
        let _ = std::io::stdin().read_line(&mut String::new());
    }
    std::process::exit(1);
}
