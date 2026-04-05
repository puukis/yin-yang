use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "yin",
    version,
    about = "Low-latency QUIC remote desktop host for Linux Wayland and Windows."
)]
pub struct Args {
    /// Socket address to bind the QUIC listener to.
    #[arg(value_name = "BIND_ADDR", default_value = "0.0.0.0:9000")]
    pub bind_addr: std::net::SocketAddr,

    /// Tracing filter passed to tracing-subscriber.
    #[arg(long, env = "RUST_LOG", default_value = "yin=debug,info")]
    pub log_filter: String,
}

#[cfg(test)]
mod tests {
    use super::Args;
    use clap::Parser;

    #[test]
    fn parses_default_values() {
        let args = Args::try_parse_from(["yin"]).expect("args should parse");
        assert_eq!(args.bind_addr.to_string(), "0.0.0.0:9000");
        assert!(!args.log_filter.is_empty());
    }

    #[test]
    fn parses_bind_addr_and_log_filter() {
        let args =
            Args::try_parse_from(["yin", "127.0.0.1:9443", "--log-filter", "info,yin=trace"])
                .expect("args should parse");
        assert_eq!(args.bind_addr.to_string(), "127.0.0.1:9443");
        assert_eq!(args.log_filter, "info,yin=trace");
    }
}
