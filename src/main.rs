#![warn(
    clippy::all,
    clippy::restriction,
    clippy::pedantic,
    clippy::nursery,
    clippy::cargo,
    rust_2018_idioms,
    rust_2018_compatibility,
    rust_2021_compatibility,
    future_incompatible,
    nonstandard_style,
    missing_copy_implementations,
    missing_debug_implementations,
    unused
)]
#![allow(
    clippy::missing_docs_in_private_items,
    clippy::implicit_return,
    clippy::mod_module_files
)]

mod network;
mod nl80211;
mod opts;
mod web;

use std::thread;

use anyhow::{Context, Result};

use clap::Parser;

use tokio::sync::oneshot;

use crate::network::{create_channel, run_network_manager_loop};
use crate::opts::Opts;
use crate::web::run_web_loop;

#[tokio::main]
async fn main() -> Result<()> {
    let opts: Opts = Opts::parse();

    let (glib_sender, glib_receiver) = create_channel();

    let (initialized_sender, initialized_receiver) = oneshot::channel();

    thread::spawn(move || {
        run_network_manager_loop(opts, initialized_sender, glib_receiver);
    });

    receive_network_initialized(initialized_receiver).await?;

    run_web_loop(glib_sender).await;

    Ok(())
}

async fn receive_network_initialized(
    initialized_receiver: oneshot::Receiver<Result<()>>,
) -> Result<()> {
    let received = initialized_receiver
        .await
        .context("Failed to receive network initialization response");

    received
        .and_then(|r| r)
        .or_else(|e| Err(e).context("Failed to initialize network"))
}
