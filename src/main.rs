fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if matches!(args.get(1).map(String::as_str), Some("--version" | "-V")) {
        println!("zmux {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    if args.len() >= 2 && args[1] == "notify" {
        let (title, body) = parse_notify_args(&args)?;
        zmux::CliServer::notify(title, body)?;
        return Ok(());
    }

    zmux::run()
}

fn parse_notify_args(args: &[String]) -> anyhow::Result<(String, String)> {
    if args.len() >= 6 && args[2] == "--title" && args[4] == "--body" {
        Ok((args[3].clone(), args[5..].join(" ")))
    } else if args.len() >= 4 && args[2] == "--title" {
        Ok((args[3].clone(), String::new()))
    } else if args.len() >= 3 {
        let title = args[2].clone();
        let body = args[3..].join(" ");
        Ok((title, body))
    } else {
        anyhow::bail!("Usage: zmux notify [--title TITLE --body BODY] [TITLE] [BODY]")
    }
}
