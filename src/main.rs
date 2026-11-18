// SPDX-FileCopyrightText: 2026 Eric Tian <thxdaemon@gmail.com>
// SPDX-License-Identifier: AGPL-3.0-or-later

mod app;
mod cli;
mod config;
mod daemon;
mod linux;
mod name;
mod protocol;
mod sandbox;

use anyhow::Result;

fn main() {
    let code = match run(cli::Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("sbox: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

fn run(cli: cli::Cli) -> Result<i32> {
    app::App::new(cli)?.execute()
}
