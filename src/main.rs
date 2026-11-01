// SPDX-License-Identifier: GPL-3.0-or-later

// Copyright (C) 2026  Forest Crossman <cyrozap@gmail.com>
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

mod cli;
mod daemon;
mod detach;
mod error;
mod hostdev;
mod monitor;
mod rule;
mod signal;
mod usb;
mod vm;

use std::process::ExitCode;

use clap::Parser;
use log::{error, info};

use crate::cli::Cli;
use crate::daemon::Daemon;
use crate::monitor::NetlinkMonitor;
use crate::usb::SysfsSource;
use crate::vm::DryRunBackend;
use crate::vm::libvirt::LibvirtBackend;

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging(verbose: u8) {
    let level = match verbose {
        0 => log::LevelFilter::Info,
        1 => log::LevelFilter::Debug,
        _ => log::LevelFilter::Trace,
    };
    env_logger::Builder::new()
        .filter_level(level)
        .parse_default_env()
        .init();
}

fn run(cli: Cli) -> error::Result<()> {
    let mut rules = Vec::with_capacity(cli.rules.len());
    for expression in &cli.rules {
        rules.push(cli::parse_rule(expression)?);
    }
    rule::resolve_bus_paths(&mut rules)?;

    if cli.detach_matching {
        let source = SysfsSource::new();
        let mut backend = LibvirtBackend::new(cli.connect.clone(), cli.domain.clone())?;
        let count = detach::detach_matching(&mut backend, &source, &rules, cli.dry_run)?;
        if cli.dry_run {
            info!("{count} matching USB hostdev(s); nothing detached (dry run)");
        } else {
            info!("detached {count} matching USB hostdev(s)");
        }
        return Ok(());
    }

    signal::install()?;

    let source = SysfsSource::new();
    let mut events = NetlinkMonitor::new()?;

    if cli.dry_run {
        info!("dry-run mode: matching devices will be logged but not attached");
        let mut daemon = Daemon::new(rules, source, DryRunBackend, true, false);
        daemon.run(&mut events)
    } else {
        let backend = LibvirtBackend::new(cli.connect.clone(), cli.domain.clone())?;
        let mut daemon = Daemon::new(rules, source, backend, false, cli.detach_on_exit);
        daemon.run(&mut events)
    }
}
