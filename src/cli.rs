use crate::location::Constraints;
use crate::store::{Name, Selector};
use crate::via::Via;
use clap::{Args, Parser, Subcommand};
use ipnet::{Ipv4Net, Ipv6Net};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version,
    about = "Run Cloudflare WARP clients side by side, and exit in the region you choose",
    long_about = "Run Cloudflare WARP clients side by side, and exit in the region you choose.\n\nEach instance lives in its own network namespace, so instances never conflict with each other or with the host's routing. Instances have an index from 0 to 255 and, optionally, a name; commands accept either. Root and each user have separate instances.\n\nTo exit in another region, pass `--location` with where you want to be and `--via` with a relay there. Waywarp connects through the relay, then moves the connection back to your own network.\n\nSet WAYWARP_LOG to a level such as `debug` for more detail. Each instance also writes a log to its state directory."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    #[command(subcommand, about = "Start an instance")]
    Up(Box<Access>),
    #[command(about = "Stop an instance")]
    Down {
        #[arg(default_value = "0", help = "Instance index or name")]
        instance: Selector,
    },
    #[command(about = "Verify and show running instances")]
    Status {
        #[arg(help = "Instance index or name; shows every running instance when omitted")]
        instance: Option<Selector>,
        #[arg(long, help = "Print one JSON object per instance")]
        json: bool,
    },
    #[command(about = "Copy an existing WARP registration into an instance")]
    Import {
        #[arg(default_value = "0", help = "Instance index or name")]
        instance: Selector,
        #[arg(
            long,
            default_value = "/var/lib/cloudflare-warp",
            help = "WARP state directory"
        )]
        from: PathBuf,
        #[arg(long, help = "Replace an existing registration")]
        replace: bool,
    },
    #[command(
        name = "warp-cli",
        about = "Run warp-cli against an instance's daemon",
        trailing_var_arg = true
    )]
    WarpCli {
        #[arg(help = "Instance index or name")]
        instance: Selector,
        #[arg(
            required = true,
            allow_hyphen_values = true,
            help = "Arguments passed to warp-cli"
        )]
        arguments: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum Access {
    #[command(about = "Serve SOCKS5 and HTTP CONNECT on loopback")]
    Proxy {
        #[command(flatten)]
        common: Up,
        #[arg(long, value_parser = loopback, help = "Listen address [default: 127.0.0.1:1080+INDEX]")]
        listen: Option<SocketAddrV4>,
    },
    #[command(about = "Link the host to WARP with a dual-stack veth pair (root only)")]
    Bridge {
        #[command(flatten)]
        common: Up,
        #[arg(long, value_parser = crate::bridge::parse4, help = "IPv4 /30 [default: derived from INDEX]")]
        subnet4: Option<Ipv4Net>,
        #[arg(long, value_parser = crate::bridge::parse6, help = "IPv6 /126 [default: derived from INDEX]")]
        subnet6: Option<Ipv6Net>,
        #[arg(
            long,
            value_enum,
            default_value = "auto",
            help = "Source NAT: follow connector routes, always translate, or never translate"
        )]
        nat: crate::bridge::nat::Mode,
    },
}

impl Access {
    pub fn common(&self) -> &Up {
        match self {
            Self::Proxy { common, .. } | Self::Bridge { common, .. } => common,
        }
    }
}

#[derive(Args, Clone)]
pub struct Up {
    #[arg(default_value = "0", help = "Instance index or name")]
    pub instance: Selector,
    #[arg(long, help = "Name the instance, replacing any earlier name")]
    pub name: Option<Name>,
    #[arg(
        long,
        help = "Keep the instance in the foreground (for service managers)"
    )]
    pub foreground: bool,
    #[arg(
        long,
        help = "Accept Cloudflare's Terms of Service when creating a WARP registration"
    )]
    pub accept_tos: bool,
    #[arg(
        long,
        value_name = "CONSTRAINTS",
        help = "Require locations, as in geo4=HK, geo6=HK, edge=HKG, probe4=HKG, or several joined with +"
    )]
    pub location: Option<Constraints>,
    #[arg(
        long,
        value_name = "VIA",
        help = "Reach the edge this way, in order, until WARP connects: direct, socks5:ADDRESS:PORT, or mudfish:FILTER (repeatable) [default: direct]"
    )]
    pub via: Vec<Via>,
    #[arg(
        long,
        help = "Physical interface [default: whichever the host routes each destination through]"
    )]
    pub interface: Option<String>,
    #[arg(
        long,
        help = "MASQUE edge address [default: registration endpoint for Teams, 162.159.198.1 otherwise]"
    )]
    pub edge: Option<Ipv4Addr>,
    #[arg(long, default_value_t = 443, value_parser = clap::value_parser!(u16).range(1..), help = "MASQUE edge port")]
    pub edge_port: u16,
    #[arg(long, default_value_t = 18081, help = "SOCKS5 port of Mudfish nodes")]
    pub mudfish_port: u16,
    #[arg(long, help = "Keep running when a required location changes")]
    pub no_rebootstrap: bool,
}

fn loopback(value: &str) -> Result<SocketAddrV4, String> {
    match value.parse::<SocketAddrV4>() {
        Ok(address) if address.ip().is_loopback() && address.port() != 0 => Ok(address),
        _ => Err("expected an IPv4 loopback address with a port, such as 127.0.0.1:1080".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parses(arguments: &[&str]) -> bool {
        Cli::try_parse_from(std::iter::once("waywarp").chain(arguments.iter().copied())).is_ok()
    }

    #[test]
    fn access_subcommands_own_their_options() {
        assert!(parses(&[
            "up",
            "proxy",
            "tokyo",
            "--listen",
            "127.0.0.2:9000"
        ]));
        assert!(parses(&["up", "bridge", "2", "--subnet4", "10.9.0.4/30"]));
        assert!(!parses(&["up", "proxy", "--subnet4", "10.9.0.4/30"]));
        assert!(!parses(&["up", "bridge", "--listen", "127.0.0.1:1080"]));
        assert!(!parses(&["up", "proxy", "--listen", "0.0.0.0:1080"]));
        assert!(!parses(&["up"]));
    }

    #[test]
    fn terms_acceptance_is_explicit_in_both_access_modes() {
        for mode in ["proxy", "bridge"] {
            for accepted in [false, true] {
                let mut arguments = vec!["waywarp", "up", mode];
                if accepted {
                    arguments.push("--accept-tos");
                }
                let Command::Up(access) = Cli::try_parse_from(arguments).unwrap().command else {
                    panic!("expected up");
                };
                assert_eq!(access.common().accept_tos, accepted);
            }
        }
    }

    #[test]
    fn warp_cli_passes_arguments_through() {
        let cli = Cli::try_parse_from(["waywarp", "warp-cli", "1", "--json", "status"]).unwrap();
        let Command::WarpCli { arguments, .. } = cli.command else {
            panic!("expected warp-cli");
        };
        assert_eq!(arguments, ["--json", "status"]);
        let cli = Cli::try_parse_from([
            "waywarp",
            "warp-cli",
            "1",
            "--accept-tos",
            "registration",
            "new",
        ])
        .unwrap();
        let Command::WarpCli { arguments, .. } = cli.command else {
            panic!("expected warp-cli");
        };
        assert_eq!(arguments, ["--accept-tos", "registration", "new"]);
    }
}
