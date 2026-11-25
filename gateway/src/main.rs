//! lattice-gateway: who may reach the Lattice running on this host.
//!
//! Lattice's browser host serves the whole app on loopback and trusts whoever
//! connects. This gateway sits in front of it, behind an HTTPS entrance
//! (Tailscale Serve, Caddy or a tunnel), and lets through only:
//!
//! - browsers paired with a one-time invite (a device credential cookie), and
//! - the owner, signed in with the recovery token.
//!
//! An optional owner socket, meant for Tailscale Serve, serves only device
//! management and trusts Serve's identity header for the configured logins.

mod agent;
mod app;
mod bridge;
mod compress;
mod downloads;
mod files;
mod pdf;
mod proxy;
mod store;
mod tex;
mod update;
mod wsdeflate;

use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(version, about = "Access gateway in front of Lattice's browser host")]
pub struct Args {
    /// Public entrance address (loopback; TLS terminates in front of it).
    #[arg(long, default_value = "127.0.0.1:18490")]
    pub listen: SocketAddr,
    /// The HTTPS origin browsers use, e.g. https://workspace.example.ts.net
    #[arg(long)]
    pub public_origin: String,
    /// Lattice's browser host.
    #[arg(long, default_value = "127.0.0.1:18452")]
    pub upstream: SocketAddr,
    /// Private directory for the paired-device registry.
    #[arg(long)]
    pub state_dir: PathBuf,
    /// File holding the owner's recovery token (created if missing).
    #[arg(long)]
    pub token_file: PathBuf,
    /// Unix socket for Tailscale Serve/Funnel in front of the workspace. Requests from
    /// the tailnet whose login is in --owner-logins are the owner, with no sign-in.
    #[arg(long)]
    pub serve_socket: Option<PathBuf>,
    /// The same for the Agent entrance.
    #[arg(long)]
    pub agent_serve_socket: Option<PathBuf>,
    /// File listing the Tailscale logins that are the owner, one per line.
    #[arg(long)]
    pub owner_logins: Option<PathBuf>,
    /// UID allowed to connect to the Tailscale sockets besides root (tailscaled in a container).
    #[arg(long)]
    pub ingress_uid: Option<u32>,
    /// Rewrite the loopback-only expressions of Lattice's page so it runs at the public origin.
    #[arg(long)]
    pub rewrite_loopback_page: bool,
    /// Directory where Lattice's xdg-open stand-in drops the addresses it was asked to open.
    #[arg(long)]
    pub open_requests: Option<PathBuf>,
    /// Folder whose subfolders the Projects page offers to open in Lattice.
    #[arg(long)]
    pub projects_root: Option<PathBuf>,
    /// Directory lattice-host and the gateway share for the owner's update
    /// prompt: what lattice-host found, and the owner's Update or Later.
    #[arg(long)]
    pub update_dir: Option<PathBuf>,
    /// Second entrance that serves Lattice's Agent UI (Synara) on an origin of its own.
    #[arg(long)]
    pub agent_listen: Option<SocketAddr>,
    /// The HTTPS origin browsers use for the Agent entrance (same host, another port).
    #[arg(long)]
    pub agent_origin: Option<String>,
    /// Only a port that a process under this path listens on is served as the Agent.
    #[arg(long)]
    pub agent_process_marker: Option<PathBuf>,
    /// tlmgr of a TeX installation the host user can write to, for the owner's TeX page.
    #[arg(long)]
    pub tlmgr: Option<PathBuf>,
    /// Tests only: run tlmgr install with --dry-run.
    #[arg(long, hide = true)]
    pub tlmgr_dry_run: bool,
    /// Plain-HTTP development only: cookies without Secure or the __Host- prefix.
    #[arg(long)]
    pub insecure_dev_cookies: bool,
}

#[tokio::main]
async fn main() {
    // `lattice-gateway --print-qr <text>`: the code lattice-host shows on the
    // host's terminal, so the host kit needs no QR library of its own.
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() == 3 && argv[1] == "--print-qr" {
        print!("{}", app::qr_text(&argv[2]));
        return;
    }
    let args = Args::parse();
    let app = match app::App::new(&args) {
        Ok(app) => Arc::new(app),
        Err(message) => {
            eprintln!("lattice-gateway: {message}");
            std::process::exit(2);
        }
    };
    if let Err(message) = app::serve(app, &args).await {
        eprintln!("lattice-gateway: {message}");
        std::process::exit(1);
    }
}
