// SPDX-License-Identifier: GPL-3.0-or-later

//! Code shared by the `mcpdoze` CLI and the `mcpdozed` background daemon:
//! running the broker ([`run`]) and the daemon's rotating log file
//! ([`logfile`]).

pub mod logfile;
pub mod run;
