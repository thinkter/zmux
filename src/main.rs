fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.get(1).is_some_and(|argument| argument == "notify") {
        let arguments = parse_notify_args(&args)?;
        let notification =
            zmux::CliNotification::new(arguments.title, arguments.subtitle, arguments.body);
        zmux::CliServer::notify(notification)?;
        return Ok(());
    }

    zmux::run_gui()
}

#[derive(Debug, PartialEq, Eq)]
struct NotifyArguments {
    title: String,
    subtitle: Option<String>,
    body: String,
}

fn parse_notify_args(args: &[String]) -> anyhow::Result<NotifyArguments> {
    const USAGE: &str =
        "Usage: zmux notify [--title TITLE] [--subtitle SUBTITLE] [--body BODY] [TITLE] [BODY ...]";

    let mut title = None;
    let mut subtitle = None;
    let mut body = None;
    let mut positional = Vec::new();
    let mut positional_only = false;
    let mut index = 2;

    while index < args.len() {
        let argument = &args[index];
        if positional_only {
            positional.push(argument.clone());
            index += 1;
            continue;
        }
        if argument == "--" {
            positional_only = true;
            index += 1;
            continue;
        }

        let (name, inline_value) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(name, value)| {
                (name, Some(value.to_owned()))
            });
        let target = match name {
            "--title" => Some((&mut title, "--title")),
            "--subtitle" => Some((&mut subtitle, "--subtitle")),
            "--body" => Some((&mut body, "--body")),
            _ => None,
        };

        if let Some((target, flag)) = target {
            if target.is_some() {
                anyhow::bail!("{flag} may only be provided once\n{USAGE}");
            }
            let value = if let Some(value) = inline_value {
                value
            } else {
                index += 1;
                args.get(index)
                    .filter(|value| !value.is_empty() && !value.starts_with("--"))
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("{flag} requires a value\n{USAGE}"))?
            };
            if value.is_empty() {
                anyhow::bail!("{flag} requires a non-empty value\n{USAGE}");
            }
            *target = Some(value);
            index += 1;
            continue;
        }

        if argument.starts_with('-') {
            anyhow::bail!("unknown notify option {argument:?}\n{USAGE}");
        }
        positional.push(argument.clone());
        index += 1;
    }

    if title.is_none() && !positional.is_empty() {
        title = Some(positional.remove(0));
    }
    if body.is_none() && !positional.is_empty() {
        body = Some(positional.join(" "));
        positional.clear();
    }
    if !positional.is_empty() {
        let suffix = positional.join(" ");
        let existing = body.get_or_insert_with(String::new);
        if !existing.is_empty() {
            existing.push(' ');
        }
        existing.push_str(&suffix);
    }
    if title.is_none() && body.is_none() {
        anyhow::bail!("{USAGE}");
    }

    Ok(NotifyArguments {
        title: title.unwrap_or_else(|| "Terminal notification".to_owned()),
        subtitle,
        body: body.unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(arguments: &[&str]) -> Vec<String> {
        ["zmux", "notify"]
            .into_iter()
            .chain(arguments.iter().copied())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn parses_positional_title_and_body() {
        assert_eq!(
            parse_notify_args(&args(&["Build finished", "All", "checks", "passed"])).unwrap(),
            NotifyArguments {
                title: "Build finished".to_owned(),
                subtitle: None,
                body: "All checks passed".to_owned(),
            }
        );
    }

    #[test]
    fn parses_named_options_in_any_order() {
        assert_eq!(
            parse_notify_args(&args(&[
                "--body",
                "All checks passed",
                "--title=Build finished",
                "--subtitle",
                "agent-2",
            ]))
            .unwrap(),
            NotifyArguments {
                title: "Build finished".to_owned(),
                subtitle: Some("agent-2".to_owned()),
                body: "All checks passed".to_owned(),
            }
        );
    }

    #[test]
    fn positional_values_fill_only_missing_named_fields() {
        assert_eq!(
            parse_notify_args(&args(&["Fallback title", "--body", "Explicit body"])).unwrap(),
            NotifyArguments {
                title: "Fallback title".to_owned(),
                subtitle: None,
                body: "Explicit body".to_owned(),
            }
        );
        assert_eq!(
            parse_notify_args(&args(&["--title", "Explicit title", "fallback", "body"])).unwrap(),
            NotifyArguments {
                title: "Explicit title".to_owned(),
                subtitle: None,
                body: "fallback body".to_owned(),
            }
        );
        assert_eq!(
            parse_notify_args(&args(&[
                "--title",
                "Explicit title",
                "--body",
                "body",
                "continuation",
            ]))
            .unwrap(),
            NotifyArguments {
                title: "Explicit title".to_owned(),
                subtitle: None,
                body: "body continuation".to_owned(),
            }
        );
    }

    #[test]
    fn body_only_uses_a_safe_default_title() {
        assert_eq!(
            parse_notify_args(&args(&["--body", "Finished"])).unwrap(),
            NotifyArguments {
                title: "Terminal notification".to_owned(),
                subtitle: None,
                body: "Finished".to_owned(),
            }
        );
    }

    #[test]
    fn double_dash_allows_dash_prefixed_positionals() {
        assert_eq!(
            parse_notify_args(&args(&["--", "-build", "done"])).unwrap(),
            NotifyArguments {
                title: "-build".to_owned(),
                subtitle: None,
                body: "done".to_owned(),
            }
        );
    }

    #[test]
    fn rejects_unknown_duplicate_and_incomplete_options() {
        assert!(parse_notify_args(&args(&["--wat", "value"])).is_err());
        assert!(parse_notify_args(&args(&["--title", "one", "--title", "two"])).is_err());
        assert!(parse_notify_args(&args(&["--subtitle"])).is_err());
        assert!(parse_notify_args(&args(&[])).is_err());
    }
}
